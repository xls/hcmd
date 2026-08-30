//! Reading a template file: the TOML shape, and the checks on the way in.
//!
//! Kept apart from the model in [`super`] because the two are read for
//! different reasons. The model is what a caller uses; this is what a person
//! writing a template argues with, and every refusal in here exists because a
//! template is data somebody hand-writes and a mistake in it would otherwise
//! be a wrong answer given confidently.

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::viewer::summary::RawSummary;

use super::{Endian, Field, FieldType, Magic, Template};

/// The TOML shape, before it is checked.
///
/// Unknown keys are refused rather than ignored: a template is data a person
/// hand-writes, and a mistyped `endain` that silently reads the file the other
/// way round would be a wrong answer given confidently.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTemplate {
    name: String,
    endian: Option<String>,
    #[serde(default)]
    offset: usize,
    magic: Option<RawMagic>,
    #[serde(default, rename = "field")]
    fields: Vec<RawField>,
    summary: Option<RawSummary>,
}

/// The `magic` table, before it is checked.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMagic {
    #[serde(default)]
    offset: usize,
    bytes: String,
}

/// One `[[field]]`, before it is checked.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawField {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    size: Option<usize>,
    endian: Option<String>,
    offset: Option<usize>,
}

/// Hex digits to bytes, whitespace ignored.
fn hex_bytes(text: &str) -> Result<Vec<u8>> {
    let digits: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if !digits.len().is_multiple_of(2) {
        return Err(Error::msg(format!(
            "{text:?} has an odd number of hex digits"
        )));
    }
    let mut out = Vec::with_capacity(digits.len() / 2);
    let raw = digits.as_bytes();
    let mut i: usize = 0;
    while let Some(pair) = raw.get(i..i.saturating_add(2)) {
        let text = std::str::from_utf8(pair).map_err(|e| Error::msg(e.to_string()))?;
        let byte = u8::from_str_radix(text, 16)
            .map_err(|_| Error::msg(format!("{text:?} is not a pair of hex digits")))?;
        out.push(byte);
        i = i.saturating_add(2);
    }
    Ok(out)
}

/// Turn one `[[field]]` into a checked [`Field`] starting at `cursor`.
fn field_from(raw: RawField, default: Endian, cursor: usize) -> Result<Field> {
    let kind = FieldType::parse(&raw.kind)?;
    let name = raw.name;
    let size = match (kind.natural_width(), raw.size) {
        (Some(natural), None) => natural,
        (Some(_), Some(_)) => {
            return Err(Error::msg(format!(
                "field {name:?}: only char and bytes take a size"
            )));
        }
        (None, Some(0)) | (None, None) => {
            return Err(Error::msg(format!(
                "field {name:?}: {} needs a size of at least one byte",
                raw.kind
            )));
        }
        (None, Some(size)) => size,
    };
    let offset = raw.offset.unwrap_or(cursor);
    if offset < cursor {
        return Err(Error::msg(format!(
            "field {name:?}: offset {offset} is behind the field before it, which ends at {cursor}"
        )));
    }
    let endian = match raw.endian {
        Some(text) => Endian::parse(&text)?,
        None => default,
    };
    Ok(Field {
        name,
        kind,
        size,
        offset,
        endian,
    })
}

impl Template {
    /// Parse one template from the text of a TOML file.
    pub fn parse(text: &str) -> Result<Self> {
        let raw: RawTemplate = toml::from_str(text).map_err(|e| Error::msg(e.to_string()))?;
        let default = match raw.endian {
            Some(ref text) => Endian::parse(text)?,
            None => Endian::default(),
        };
        let magic = match raw.magic {
            Some(magic) => Some(Magic {
                offset: magic.offset,
                bytes: hex_bytes(&magic.bytes)?,
            }),
            None => None,
        };
        let mut fields = Vec::with_capacity(raw.fields.len());
        let mut cursor = 0_usize;
        for raw_field in raw.fields {
            let field = field_from(raw_field, default, cursor)?;
            cursor = field.offset.saturating_add(field.size);
            fields.push(field);
        }
        let summary = match raw.summary {
            Some(section) => {
                let names: Vec<String> = fields.iter().map(|f| f.name.clone()).collect();
                Some(section.check(&raw.name, &names)?)
            }
            None => None,
        };
        Ok(Self {
            name: raw.name,
            offset: raw.offset,
            magic,
            fields,
            span: cursor,
            summary,
        })
    }
}
