//! What a file *is*, in a sentence a person reads.
//!
//! The field list in [`super::template`] is mechanical and complete: every
//! field, in order, decoded. That is the right answer to "what is at byte 24"
//! and the wrong answer to "what is this file". Someone opening a PNG wants
//! `1920 x 1080` and `8-bit RGBA`, not `colour_type: 6`, and someone opening
//! an AVI wants `H264` rather than the number those four bytes make.
//!
//! So a template may carry a second, optional layer that says which of its
//! fields are worth a person's attention and how to write them out. A template
//! without one simply has no summary; nothing else about it changes.
//!
//! # The `[summary]` section
//!
//! ```toml
//! [summary]
//! title = "PNG image"          # the heading; the template's name by default
//!
//! [[summary.line]]
//! label = "Dimensions"
//! field = "width"
//! with  = "height"             # two fields, one fact
//! join  = " x "                # what goes between them
//! unit  = "px"
//!
//! [[summary.line]]
//! label  = "Colour"
//! field  = "colour_type"
//! render = "enum"
//! enum   = { 0 = "greyscale", 2 = "RGB", 6 = "RGBA" }
//!
//! [[summary.line]]
//! label  = "Sample rate"
//! field  = "sample_rate"
//! render = "si"
//! unit   = "Hz"                # 44100 reads as 44.1 kHz
//!
//! [[summary.line]]
//! label  = "Codec"
//! field  = "handler"
//! render = "fourcc"            # four bytes as their four characters
//!
//! [[summary.line]]
//! label  = "Attributes"
//! field  = "characteristics"
//! render = "flags"             # the names of the bits that are set
//! flags  = { 2 = "executable", 8192 = "DLL" }
//! ```
//!
//! `render` is one of `value` (the default), `text`, `enum`, `flags`,
//! `fourcc`, `size`, `si`, `hex` and `time`. `scale` multiplies before
//! rendering, for a field that counts 16 KiB units rather than single bytes.
//! `base` reads a field that holds its number as text - every number in a tar
//! header is ASCII octal - so `base = 8` with `render = "size"` turns
//! `00000000144` into `100 B`.
//!
//! Where a rendering cannot be applied - an `enum` with no entry for 27, a
//! number the file ended inside - the raw value is shown. Inventing a name
//! would be worse than the number, and hiding the line would leave a person
//! wondering what was there.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::error::{Error, Result};

use super::summary_render::{Cell, Render, Style};
use super::template::{Template, applied};

/// One rendered line: what it is called, and what it says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryLine {
    /// The label on the left.
    pub label: String,
    /// The value on the right, already rendered.
    pub value: String,
}

/// One line of a template's summary, as the template declared it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineSpec {
    /// The label shown on the left.
    pub label: String,
    /// The field this line reads.
    pub field: String,
    /// A second field, joined to the first.
    pub with: Option<String>,
    /// What goes between the two.
    pub join: String,
    /// How the value is written out.
    pub style: Style,
}

/// A template's summary: a heading and the lines under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// The heading, which is the template's name unless it says otherwise.
    pub title: String,
    /// The lines, in the order they are shown.
    pub lines: Vec<LineSpec>,
}

/// The `[summary]` section, before it is checked.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawSummary {
    title: Option<String>,
    #[serde(default, rename = "line")]
    lines: Vec<RawLine>,
}

/// One `[[summary.line]]`, before it is checked.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLine {
    label: String,
    field: String,
    with: Option<String>,
    join: Option<String>,
    render: Option<String>,
    unit: Option<String>,
    scale: Option<u64>,
    #[serde(rename = "enum")]
    values: Option<BTreeMap<String, String>>,
    flags: Option<BTreeMap<String, String>>,
    base: Option<u32>,
}

/// A table of decimal keys to names, with the keys parsed.
fn table<T>(
    raw: Option<BTreeMap<String, String>>,
    what: &str,
    label: &str,
) -> Result<BTreeMap<T, String>>
where
    T: Ord + std::str::FromStr,
{
    let mut out = BTreeMap::new();
    for (key, name) in raw.unwrap_or_default() {
        let Ok(parsed) = key.parse::<T>() else {
            return Err(Error::msg(format!(
                "summary line {label:?}: {key:?} is not a number, and every {what} key is one"
            )));
        };
        out.insert(parsed, name);
    }
    Ok(out)
}

impl RawSummary {
    /// Check this section against the template's own fields.
    ///
    /// A line that names a field the template does not have is refused rather
    /// than dropped. A dropped line is a summary that is quietly missing the
    /// fact it was written for, and the mistake is a typo that nothing else
    /// would ever report.
    pub(crate) fn check(self, name: &str, fields: &[String]) -> Result<Summary> {
        let mut lines = Vec::with_capacity(self.lines.len());
        for raw in self.lines {
            let label = raw.label;
            for named in [Some(&raw.field), raw.with.as_ref()].into_iter().flatten() {
                if !fields.iter().any(|f| f == named) {
                    return Err(Error::msg(format!(
                        "summary line {label:?}: there is no field called {named:?}"
                    )));
                }
            }
            let render = match raw.render {
                Some(ref text) => Render::parse(text).ok_or_else(|| {
                    Error::msg(format!(
                        "summary line {label:?}: {text:?} is not a rendering; use one of value text enum flags fourcc size si hex"
                    ))
                })?,
                None => Render::default(),
            };
            let style = Style {
                render,
                unit: raw.unit,
                scale: raw.scale,
                values: table(raw.values, "enum", &label)?,
                bits: table(raw.flags, "flags", &label)?,
                base: raw.base,
            };
            lines.push(LineSpec {
                label,
                field: raw.field,
                with: raw.with,
                join: raw.join.unwrap_or_else(|| " x ".to_string()),
                style,
            });
        }
        Ok(Summary {
            title: self.title.unwrap_or_else(|| name.to_string()),
            lines,
        })
    }
}

/// Every line of `template`'s summary that `bytes` reaches.
///
/// `bytes` starts at the file's first byte; the structure is read from the
/// template's own offset, so a superblock at 1024 or a volume descriptor at
/// 32768 is found without the caller knowing where it is. A line whose field
/// the file does not reach is left out, which is what a truncated file gets
/// rather than a line of nothing.
#[must_use]
pub fn summary(template: &Template, bytes: &[u8]) -> Vec<SummaryLine> {
    let Some(spec) = template.summary.as_ref() else {
        return Vec::new();
    };
    let readings = applied(template, bytes, template.offset);
    let mut out = Vec::with_capacity(spec.lines.len());
    for line in &spec.lines {
        let Some(value) = render_line(line, &readings, bytes) else {
            continue;
        };
        out.push(SummaryLine {
            label: line.label.clone(),
            value,
        });
    }
    out
}

/// One line's value, or `None` where the file did not reach its field.
fn render_line(
    line: &LineSpec,
    readings: &[super::template::FieldReading],
    bytes: &[u8],
) -> Option<String> {
    let first = cell(&line.field, readings, bytes, line.style.base)?;
    let head = line.style.render(first);
    let Some(second) = line.with.as_ref() else {
        return Some(head);
    };
    let tail = line
        .style
        .render(cell(second, readings, bytes, line.style.base)?);
    // The unit belongs to the pair, not to each half: `1920 x 1080 px` and
    // never `1920 px x 1080 px`.
    let unit = match line.style.unit.as_deref() {
        Some(unit) if !unit.is_empty() => format!(" {unit}"),
        _ => String::new(),
    };
    let strip = |text: String| text.trim_end_matches(&unit).trim_end().to_string();
    Some(format!("{}{}{}{unit}", strip(head), line.join, strip(tail)))
}

/// The decoded field called `name`, ready to render.
fn cell<'a>(
    name: &str,
    readings: &'a [super::template::FieldReading],
    bytes: &'a [u8],
    base: Option<u32>,
) -> Option<Cell<'a>> {
    let reading = readings.iter().find(|r| r.name == name)?;
    let start = usize::try_from(reading.offset).ok()?;
    let end = start.saturating_add(reading.size);
    let number = match base {
        // A field that holds its number as text: a tar header writes every one
        // of its numbers as ASCII octal, and the digits have to be read before
        // anything can be said about them.
        Some(radix) => {
            let digits = reading.value.trim_matches('"').replace('.', " ");
            i128::from_str_radix(digits.trim(), radix).ok()
        }
        // The field list has already decoded the number; parsing its own
        // output back is what keeps one decoder rather than two that could
        // round differently. A float, a text field or a truncated number does
        // not parse, and every rendering falls back to the raw value for it.
        None => reading.value.parse::<i128>().ok(),
    };
    Some(Cell {
        raw: reading.value.as_str(),
        number,
        bytes: bytes.get(start..end).unwrap_or_default(),
    })
}

/// The heading for a template: its summary's title, or its own name.
#[must_use]
pub fn heading(template: &Template) -> &str {
    match template.summary.as_ref() {
        Some(spec) => spec.title.as_str(),
        None => template.name.as_str(),
    }
}

/// Which template `head` looks like, or `None` where none of them claim it.
///
/// The longest magic wins where more than one matches, because a longer magic
/// is a narrower claim: a RAR 5 file matches the seven-byte RAR 4 signature as
/// well as its own eight-byte one, an `ftyp` box matches both the generic ISO
/// base media template and any that names a brand, and in each pair the longer
/// magic is the one that knew more about the file. Ties keep the first, and
/// the list is sorted by name, so the answer does not depend on the order the
/// directory happened to be read in.
#[must_use]
pub fn best_match<'a>(templates: &'a [Template], head: &[u8]) -> Option<&'a Template> {
    templates
        .iter()
        .filter(|t| super::template::matches(t, head))
        // `min_by_key` on the reversed length rather than `max_by_key`: the
        // maximum of a tie is the *last* equal element, and the first is the
        // one this promises.
        .min_by_key(|t| std::cmp::Reverse(t.magic.as_ref().map_or(0, |m| m.bytes.len())))
}

#[cfg(test)]
#[path = "summary_tests.rs"]
mod tests;
