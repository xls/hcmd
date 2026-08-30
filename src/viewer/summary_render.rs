//! Turning one field's bytes into one human sentence.
//!
//! Everything in here answers the same question in a different way: the file
//! holds the number 6, and a person wants to read `RGBA`. The rules are all
//! declared in the template, so this module has no knowledge of any format in
//! it - which is what keeps a new format a data change rather than a code
//! change.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use crate::panel::format::human_size;
use crate::viewer::hex::ascii_glyph;

/// How one line's value is written out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Render {
    /// The number as the field list already decoded it.
    #[default]
    Value,
    /// Text, with the quotes and the trailing padding taken off.
    Text,
    /// A name from the line's `enum` table.
    Enum,
    /// The names of the bits that are set, from the line's `flags` table.
    Flags,
    /// Four bytes as the four characters they are written as.
    FourCc,
    /// A size, scaled to KB, MB and up.
    Size,
    /// A number scaled by thousands, so 44100 Hz reads as 44.1 kHz.
    Si,
    /// Hex, with an `0x` in front.
    Hex,
    /// A Unix timestamp, as a date.
    Time,
}

impl Render {
    /// The name as it is written in a template.
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "value" => Self::Value,
            "text" => Self::Text,
            "enum" => Self::Enum,
            "flags" => Self::Flags,
            "fourcc" => Self::FourCc,
            "size" => Self::Size,
            "si" => Self::Si,
            "hex" => Self::Hex,
            "time" => Self::Time,
            _ => return None,
        })
    }
}

/// A size in bytes, as a person writes one.
///
/// [`human_size`] is what the panel's size column uses, so the two agree; the
/// `B` is appended because a summary line is read on its own, without a column
/// heading above it saying what the number counts.
fn size(value: u64) -> String {
    if value < 1024 {
        return format!("{value} B");
    }
    format!("{}B", human_size(value))
}

/// A number scaled by thousands, with `unit` after the prefix.
///
/// Thousands and not 1024s: this is for rates and frequencies, where kilo has
/// always meant a thousand, and a sample rate written as 43.1 kHz would simply
/// be wrong.
fn si(value: i128, unit: &str) -> String {
    const STEPS: [(i128, &str); 3] = [(1_000_000_000, "G"), (1_000_000, "M"), (1_000, "k")];
    let magnitude = value.abs();
    for (step, prefix) in STEPS {
        if magnitude >= step {
            // One decimal always, and then the trailing `.0` taken off: 44100
            // Hz is 44.1 kHz and 48000 Hz is 48 kHz, and rounding the first to
            // 44 would lose the half of the number a person came to read.
            let text = format!("{:.1}", value as f64 / step as f64);
            let text = text.trim_end_matches(".0");
            return format!("{text} {prefix}{unit}");
        }
    }
    format!("{value} {unit}")
}

/// A Unix timestamp as a date, or `None` where the number is not one.
///
/// The window is 1970 to 2100. Outside it the number is not a time anyone
/// stored on purpose, and a line reading "1901-12-13" for a field that happens
/// to hold -1 would be a confident wrong answer.
fn time(secs: i128) -> Option<String> {
    const TO: i128 = 4_102_444_800; // 2100-01-01
    if !(0..TO).contains(&secs) {
        return None;
    }
    let secs = i64::try_from(secs).ok()?;
    DateTime::<Utc>::from_timestamp(secs, 0).map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
}

/// The names of every bit of `value` that the table has a name for.
///
/// A mask rather than a bit index, so a two-bit field with four meanings can
/// be given one name per combination. A mask matches when every bit in it is
/// set, which is why the table is walked in mask order and not in value order.
fn flags(value: u64, names: &BTreeMap<u64, String>) -> String {
    let mut found: Vec<&str> = Vec::new();
    for (mask, name) in names {
        if *mask != 0 && value & mask == *mask {
            found.push(name.as_str());
        }
    }
    if found.is_empty() {
        return "none".to_string();
    }
    found.join(", ")
}

/// Four bytes as the four characters they stand for.
fn fourcc(bytes: &[u8]) -> String {
    let text: String = bytes.iter().map(|b| ascii_glyph(*b)).collect();
    text.trim_end().to_string()
}

/// Text with the field list's quotes and the format's padding taken off.
///
/// A fixed-width name field is padded with spaces or with zero bytes, and
/// neither belongs in a sentence a person reads.
fn text(raw: &str) -> String {
    raw.trim_matches('"')
        .trim_end_matches('.')
        .trim()
        .to_string()
}

/// What one line needs in order to render: the decoded value both ways.
#[derive(Debug, Clone, Copy)]
pub struct Cell<'a> {
    /// The value as the field list decoded it, quotes and all.
    pub raw: &'a str,
    /// The same value as a number, where it is one.
    pub number: Option<i128>,
    /// The field's own bytes, for the renderings that read them directly.
    pub bytes: &'a [u8],
}

/// How one line renders its value, as the template declared it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Style {
    /// Which rendering to use.
    pub render: Render,
    /// A unit written after the value.
    pub unit: Option<String>,
    /// A multiplier applied before scaling, for a field that counts something
    /// other than single bytes.
    pub scale: Option<u64>,
    /// The names for `enum`.
    pub values: BTreeMap<i128, String>,
    /// The names for `flags`.
    pub bits: BTreeMap<u64, String>,
    /// The radix a text field's digits are in, where the field holds a number
    /// written out rather than a binary one. A tar header's every number is
    /// ASCII octal, which is what this is for.
    pub base: Option<u32>,
}

impl Style {
    /// Render one value.
    ///
    /// Every path has the same fallback: where the rendering cannot be
    /// applied - an enum with no entry for 27, a number that did not decode
    /// because the file ended inside it - the raw value is shown. A summary
    /// that invented a name would be worse than the number, and one that hid
    /// the line would leave a person wondering what was there.
    pub fn render(&self, cell: Cell<'_>) -> String {
        let scaled = cell
            .number
            .map(|n| n.saturating_mul(i128::from(self.scale.unwrap_or(1))));
        let body = match self.render {
            Render::Value => match scaled {
                Some(n) => n.to_string(),
                None => cell.raw.to_string(),
            },
            Render::Text => return text(cell.raw),
            Render::Enum => match scaled.and_then(|n| self.values.get(&n)) {
                Some(name) => return name.clone(),
                None => return self.plain(cell),
            },
            Render::Flags => match scaled.and_then(|n| u64::try_from(n).ok()) {
                Some(n) => return flags(n, &self.bits),
                None => return self.plain(cell),
            },
            Render::FourCc => return fourcc(cell.bytes),
            Render::Size => match scaled.and_then(|n| u64::try_from(n).ok()) {
                Some(n) => return size(n),
                None => return self.plain(cell),
            },
            Render::Si => match scaled {
                Some(n) => return si(n, self.unit.as_deref().unwrap_or_default()),
                None => return self.plain(cell),
            },
            Render::Hex => match scaled.and_then(|n| u64::try_from(n).ok()) {
                Some(n) => format!("0x{n:X}"),
                None => cell.raw.to_string(),
            },
            Render::Time => match scaled.and_then(time) {
                Some(text) => return text,
                None => return self.plain(cell),
            },
        };
        self.with_unit(body)
    }

    /// The value with nothing applied to it but the unit, which is what every
    /// rendering falls back to.
    fn plain(&self, cell: Cell<'_>) -> String {
        self.with_unit(match cell.number {
            Some(n) => n.to_string(),
            None => cell.raw.to_string(),
        })
    }

    /// `body`, with the line's unit after it where it has one.
    fn with_unit(&self, body: String) -> String {
        match self.unit.as_deref() {
            Some(unit) if !unit.is_empty() => format!("{body} {unit}"),
            _ => body,
        }
    }
}

#[cfg(test)]
#[path = "summary_render_tests.rs"]
mod tests;
