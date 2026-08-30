//! Applying a template: reading a file through it, and recognising one.
//!
//! The half of [`super`] that touches bytes. The other half is the format and
//! its parser; this is what a caller actually runs per file and per frame, and
//! it is here rather than beside the parser because the two are read for
//! different reasons - one when writing a template, one when using it.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::viewer::hex::ascii_glyph;
use crate::viewer::inspect;

use super::{Extent, Field, FieldReading, FieldSpan, FieldType, Template};

impl Template {
    /// Read a template from a file, naming the file if it will not parse.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        Self::parse(&text).map_err(|e| Error::Config {
            file: path.to_path_buf(),
            message: e.to_string(),
        })
    }
}

/// Every `*.toml` under `dir`, including its subdirectories, sorted by name.
///
/// Subdirectories because the shipped set is grouped by what the formats are -
/// `image/`, `fs/`, `exe/` - and that grouping is for the person reading the
/// repository, not something the picker should have to know about. A file that
/// will not parse is skipped with its reason rather than failing the walk: one
/// bad template must not cost a user every other one.
pub fn load_dir(dir: &Path) -> (Vec<Template>, Vec<String>) {
    let mut found = Vec::new();
    let mut problems = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    let mut files: Vec<PathBuf> = Vec::new();
    while let Some(here) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&here) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "toml") {
                files.push(path);
            }
        }
    }
    files.sort();
    for path in files {
        match Template::load(&path) {
            Ok(template) => found.push(template),
            Err(e) => problems.push(e.to_string()),
        }
    }
    found.sort_by_key(|t| t.name.to_lowercase());
    (found, problems)
}

/// Does `head`, read from the start of a file, look like this format?
///
/// False for a template that declares no `magic`: it has said nothing about
/// how it is recognised, and a template that claimed every file would be worse
/// than one that claims none. False, too, where `head` is too short to hold
/// the bytes the magic is at, which is the honest answer for a file whose
/// signature has not been read yet.
#[must_use]
pub fn matches(template: &Template, head: &[u8]) -> bool {
    let Some(magic) = template.magic.as_ref() else {
        return false;
    };
    let end = magic.offset.saturating_add(magic.bytes.len());
    head.get(magic.offset..end) == Some(magic.bytes.as_slice())
}

/// What a number reads as when the file ends inside it.
///
/// A number needs all of its bytes to be a number at all, so there is nothing
/// honest to print. `char` and `bytes` are not in this position: half of a
/// name is still half of a name, and it is shown.
const TRUNCATED: &str = "(truncated)";

/// One field's bytes, rendered the way a person reads them.
fn value_of(field: &Field, bytes: &[u8]) -> String {
    let big = field.endian.is_big();
    match field.kind {
        FieldType::Unsigned(width) => inspect::word(bytes, width, big)
            .map(|v| v.to_string())
            .unwrap_or_else(|| TRUNCATED.to_string()),
        FieldType::Signed(width) => inspect::word(bytes, width, big)
            .map(|v| inspect::signed(v, width).to_string())
            .unwrap_or_else(|| TRUNCATED.to_string()),
        FieldType::Float(4) => inspect::word(bytes, 4, big)
            .map(|v| inspect::float(f64::from(f32::from_bits(v as u32))))
            .unwrap_or_else(|| TRUNCATED.to_string()),
        FieldType::Float(_) => inspect::word(bytes, 8, big)
            .map(|v| inspect::float(f64::from_bits(v)))
            .unwrap_or_else(|| TRUNCATED.to_string()),
        FieldType::Char => {
            let text: String = bytes.iter().map(|b| ascii_glyph(*b)).collect();
            format!("\"{text}\"")
        }
        FieldType::Bytes => bytes.iter().map(|b| format!("{b:02X}")).collect(),
    }
}

/// Decode every field of `template` that `bytes` reaches, from `at`.
///
/// `at` is where the structure starts inside `bytes`, and the offsets that
/// come back are indexes into that same buffer, so a caller reading a window
/// of a larger file adds the window's own base to report an absolute offset.
///
/// A file that ends inside a field yields that field with the size it really
/// has and then stops, and one that ends before a field starts simply does not
/// mention it. Neither is an error: a template is applied wherever the cursor
/// is, and the last header in a file is usually cut off.
#[must_use]
pub fn applied(template: &Template, bytes: &[u8], at: usize) -> Vec<FieldReading> {
    let mut out = Vec::with_capacity(template.fields.len());
    for field in &template.fields {
        let start = at.saturating_add(field.offset);
        let end = start.saturating_add(field.size);
        let Some(slice) = bytes.get(start..end).or_else(|| bytes.get(start..)) else {
            break;
        };
        if slice.is_empty() && field.size > 0 {
            break;
        }
        let short = slice.len() < field.size;
        out.push(FieldReading {
            name: field.name.clone(),
            offset: start as u64,
            size: slice.len(),
            value: value_of(field, slice),
        });
        if short {
            break;
        }
    }
    out
}

/// Which field covers `offset`, or `None` where none does.
///
/// A binary search rather than a scan, and the reason is the caller: the hex
/// renderer asks this once per byte it draws, which is a thousand times a
/// frame, and a template can have a hundred fields. A scan would make that
/// 100,000 comparisons a frame; this makes it 700. The search is sound because
/// [`applied`] and [`extents`] both emit their fields in ascending offset
/// order and never overlapping, so the last one that starts at or before
/// `offset` is the only one that can contain it - everything after it starts
/// later, and everything before it ended before that one began.
#[must_use]
pub fn field_at<T: Extent>(fields: &[T], offset: u64) -> Option<&T> {
    let after = fields.partition_point(|f| f.first_byte() <= offset);
    let found = fields.get(after.checked_sub(1)?)?;
    let end = found.first_byte().saturating_add(found.byte_len() as u64);
    (offset >= found.first_byte() && offset < end).then_some(found)
}

/// Where every field of `template` falls when it is applied at `at`.
///
/// No bytes are read and none are needed: a field's offset and width are the
/// template's, not the file's. `len` is the file's length, and it is what
/// stops a span pointing at a byte that is not there - a field the file ends
/// inside comes back with the part that exists, and one that starts past the
/// end is not mentioned at all.
#[must_use]
pub fn extents(template: &Template, at: u64, len: u64) -> Vec<FieldSpan> {
    let mut out = Vec::with_capacity(template.fields.len());
    for field in &template.fields {
        let start = at.saturating_add(field.offset as u64);
        if start >= len {
            break;
        }
        let end = start.saturating_add(field.size as u64).min(len);
        let size = usize::try_from(end.saturating_sub(start)).unwrap_or(0);
        if size == 0 {
            break;
        }
        out.push(FieldSpan {
            name: field.name.clone(),
            offset: start,
            size,
        });
    }
    out
}

/// Which parts of the `len` bytes at `from` are covered, as indexes into that
/// run.
///
/// This is the renderer's question, asked once per row rather than once per
/// byte: it wants the runs to paint, not a field name. Adjacent fields come
/// back merged, because a header whose every byte is named is one coloured run
/// on screen and not fourteen abutting ones, and the span list is walked from
/// a binary search rather than from the front - a row in the middle of a
/// hundred-field structure must not cost a hundred comparisons.
#[must_use]
pub fn coverage<T: Extent>(fields: &[T], from: u64, len: usize) -> Vec<std::ops::Range<usize>> {
    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    let row_end = from.saturating_add(len as u64);
    // The first field that reaches into this row. Ends ascend with starts,
    // because the fields do not overlap, which is what makes this sound.
    let first =
        fields.partition_point(|f| f.first_byte().saturating_add(f.byte_len() as u64) <= from);
    let Some(rest) = fields.get(first..) else {
        return out;
    };
    for field in rest {
        let start = field.first_byte();
        if start >= row_end {
            break;
        }
        let end = start.saturating_add(field.byte_len() as u64).min(row_end);
        let lo = usize::try_from(start.max(from).saturating_sub(from)).unwrap_or(0);
        let hi = usize::try_from(end.saturating_sub(from)).unwrap_or(0);
        if lo >= hi {
            continue;
        }
        match out.last_mut() {
            // Abutting the run before it: one run, not two.
            Some(last) if last.end >= lo => last.end = last.end.max(hi),
            _ => out.push(lo..hi),
        }
    }
    out
}
