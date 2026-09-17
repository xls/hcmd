//! Binary struct templates: saying what a region of a file *is*.
//!
//! The hex dump shows bytes. A template says that the four bytes at 16 are
//! `width` and read 1920, which is the question someone opens a hex viewer
//! with. A template is a small TOML file naming the fields of a known format
//! in order; the ones this repository ships live in `templates/`, and more can
//! be fetched the way themes are.
//!
//! # The format
//!
//! ```toml
//! name   = "PNG"
//! endian = "big"        # the default byte order for every field
//! offset = 0            # where in a file this structure normally begins
//!
//! # Optional. How the format is recognised, as an absolute file offset and
//! # the bytes that must be there.
//! magic = { offset = 0, bytes = "89504E470D0A1A0A" }
//!
//! [[field]]
//! name = "signature"
//! type = "bytes"
//! size = 8
//!
//! [[field]]
//! name = "ihdr_length"
//! type = "u32"
//!
//! [[field]]
//! name   = "width"
//! type   = "u32"
//! endian = "big"        # optional, overrides the template's default
//! offset = 16           # optional, relative to the start of the structure
//! ```
//!
//! Types are `u8 u16 u32 u64 i8 i16 i32 i64 f32 f64 char bytes`. The numeric
//! ones take their natural width and must not declare a `size`; `char` and
//! `bytes` have no natural width and must. `char` reads as text, `bytes` as
//! hex.
//!
//! Fields follow one another with no gaps unless one declares an `offset`,
//! which is relative to the start of the structure rather than to the file so
//! that the same template can be applied anywhere - to a partition's
//! superblock inside a disk image, say, and not only to the image's own
//! byte 1024. An `offset` may skip forward over reserved bytes; it may not
//! move backwards, which would let two fields claim the same byte.
//!
//! # Chunked formats
//!
//! Some formats do not put their fields at fixed offsets. A RIFF file - WAV,
//! AVI, WebP - is a header and then a run of chunks, each `[four-byte id][u32
//! size][payload]`, and a `JUNK` or `bext` chunk a writer inserts moves
//! everything after it. A template declares how to walk the chunks and its
//! fields anchor to one by id, so the walk finds the field wherever the chunk
//! turns out to be:
//!
//! ```toml
//! # After the 12-byte RIFF header, the file is a run of RIFF chunks.
//! chunks = { after = 12, scheme = "riff" }
//!
//! [[field]]
//! name   = "sample_rate"
//! type   = "u32"
//! chunk  = "fmt "       # this field lives in the `fmt ` chunk
//! offset = 12           # from the chunk's first byte: id 0, size 4, payload 8
//! ```
//!
//! A chunk field's `offset` is measured from the chunk's own first byte - its
//! four-byte id - so `8` is the first byte of the payload. A field whose chunk
//! the file does not contain is simply not read, the same as one past the end.
//! The one scheme today is `riff`; IFF, AIFF and PNG are the same idea with a
//! different size encoding and belong here as more schemes when a format needs
//! them.
//!
//! # Why truncation is not an error
//!
//! A file shorter than the template is the ordinary case, not the broken one:
//! a template is applied at the cursor, and the last header in a file is often
//! cut off. [`applied`] returns the fields that fit and stops, so a partial
//! structure reads as far as it goes rather than as nothing at all. A field
//! the file ends in the middle of comes back with the size it really has, so
//! nothing downstream points at a byte that is not there.

use crate::error::{Error, Result};
use crate::viewer::summary::Summary;

/// The byte order a field is read in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Endian {
    /// Least significant byte first.
    #[default]
    Little,
    /// Most significant byte first.
    Big,
}

impl Endian {
    /// `little`, `le`, `big`, `be`, in any case.
    fn parse(text: &str) -> Result<Self> {
        match text.to_ascii_lowercase().as_str() {
            "little" | "le" => Ok(Self::Little),
            "big" | "be" => Ok(Self::Big),
            other => Err(Error::msg(format!(
                "{other:?} is not a byte order; use \"little\" or \"big\""
            ))),
        }
    }

    /// True for [`Endian::Big`], which is how [`inspect::word`] asks.
    const fn is_big(self) -> bool {
        matches!(self, Self::Big)
    }
}

/// What one field holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    /// An unsigned integer of 1, 2, 4 or 8 bytes.
    Unsigned(usize),
    /// A signed integer of 1, 2, 4 or 8 bytes.
    Signed(usize),
    /// An IEEE 754 float of 4 or 8 bytes.
    Float(usize),
    /// Text, shown quoted, non-printables as `.`.
    Char,
    /// Opaque bytes, shown as hex.
    Bytes,
}

impl FieldType {
    /// The type name as it is written in a template.
    fn parse(text: &str) -> Result<Self> {
        Ok(match text {
            "u8" => Self::Unsigned(1),
            "u16" => Self::Unsigned(2),
            "u32" => Self::Unsigned(4),
            "u64" => Self::Unsigned(8),
            "i8" => Self::Signed(1),
            "i16" => Self::Signed(2),
            "i32" => Self::Signed(4),
            "i64" => Self::Signed(8),
            "f32" => Self::Float(4),
            "f64" => Self::Float(8),
            "char" => Self::Char,
            "bytes" => Self::Bytes,
            other => {
                return Err(Error::msg(format!(
                    "{other:?} is not a field type; use one of u8 u16 u32 u64 i8 i16 i32 i64 f32 f64 char bytes"
                )));
            }
        })
    }

    /// The width the type carries on its own, or `None` where the template
    /// has to declare one.
    const fn natural_width(self) -> Option<usize> {
        match self {
            Self::Unsigned(n) | Self::Signed(n) | Self::Float(n) => Some(n),
            Self::Char | Self::Bytes => None,
        }
    }
}

/// One named field of a structure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// What the field is called, as the reading is labelled.
    pub name: String,
    /// What it holds.
    pub kind: FieldType,
    /// How many bytes it occupies. Never zero.
    pub size: usize,
    /// Where it starts. Relative to the start of the structure, or - when
    /// [`Field::chunk`] is set - relative to the start of that chunk, its
    /// four-byte id included, so `offset = 8` is the first byte of the
    /// chunk's payload.
    pub offset: usize,
    /// The chunk this field lives in, for a format whose fields are not at
    /// fixed positions but inside content-addressed chunks (a RIFF `fmt `, a
    /// `data`). `None` for a field at a fixed offset from the structure start,
    /// which is every field of a format that has no [`Template::chunks`]. A
    /// field whose chunk the file does not contain is simply not read.
    pub chunk: Option<[u8; 4]>,
    /// The byte order it is read in, the template's default already applied.
    pub endian: Endian,
}

/// How a container lays its chunks out, for a format whose fields live inside
/// content-addressed chunks rather than at fixed offsets.
///
/// One scheme today and room for more: IFF and AIFF are the same walk with a
/// big-endian size, and PNG puts the length before the id. Each is a way of
/// answering the one question a [`Field::chunk`] asks - where does the chunk
/// with this id begin - so they belong together behind one enum rather than as
/// scattered special cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkScheme {
    /// RIFF: a flat run of `[four-byte id][little-endian u32 size][size bytes
    /// of payload][a pad byte when the size is odd]`. WAV, AVI, WebP and ANI.
    Riff,
}

impl ChunkScheme {
    /// The scheme name as it is written in a template.
    fn parse(text: &str) -> Result<Self> {
        match text {
            "riff" => Ok(Self::Riff),
            other => Err(Error::msg(format!(
                "{other:?} is not a chunk scheme; the schemes are: riff"
            ))),
        }
    }
}

/// How to find the chunks a [`Field::chunk`] names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunks {
    /// Where the chunk run begins, relative to the start of the structure. The
    /// twelve-byte RIFF header comes first, so a WAV's is `12`.
    pub after: usize,
    /// How the chunks are laid out.
    pub scheme: ChunkScheme,
}

/// How a format is recognised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Magic {
    /// The absolute file offset the bytes must be at.
    pub offset: usize,
    /// The bytes themselves.
    pub bytes: Vec<u8>,
}

/// A parsed template: a named format and the fields it is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    /// The name shown in the picker.
    pub name: String,
    /// Where in a file this structure normally begins. Zero for a header that
    /// is at the front, 32768 for an ISO 9660 primary volume descriptor.
    pub offset: usize,
    /// How the format is recognised, where it declares a way.
    pub magic: Option<Magic>,
    /// How to walk the format's chunks, for a format whose fields live inside
    /// content-addressed chunks. `None` for a fixed-layout format, which is
    /// most of them.
    pub chunks: Option<Chunks>,
    /// The fields, in the order they were declared.
    pub fields: Vec<Field>,
    /// How many bytes the whole structure spans, from its start to the end of
    /// its last field.
    pub span: usize,
    /// The plain-language summary, where the template carries one.
    ///
    /// Optional and separate from [`Template::fields`] on purpose: the fields
    /// are the mechanical truth and are always there, and this is the second
    /// layer that says which of them a person came to read. See
    /// [`crate::viewer::summary`].
    pub summary: Option<Summary>,
}

/// A run of bytes something occupies.
///
/// Two things in here are one: a decoded [`FieldReading`] and a bare
/// [`FieldSpan`]. The renderer wants extents and nothing else, the summary
/// wants values as well, and [`field_at`](crate::viewer::template::field_at)
/// is the same binary search either way. One trait rather than two copies of
/// it is what keeps them from disagreeing about which byte belongs to which
/// field.
pub trait Extent {
    /// The offset of its first byte.
    fn first_byte(&self) -> u64;
    /// How many bytes it covers. Never zero for a field of a template.
    fn byte_len(&self) -> usize;
}

/// Where one field falls in the file, with no value read for it.
///
/// The hex renderer colours the bytes a template explains, and to do that it
/// needs where they are and nothing else. Field offsets and sizes are fixed by
/// the template, so this can be built without reading a single byte - which is
/// what lets the colouring survive scrolling past the end of what has been
/// read, and what keeps it off the draw path's budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSpan {
    /// The field's name.
    pub name: String,
    /// Its first byte, as a file offset.
    pub offset: u64,
    /// How many bytes it covers, already clipped to the file's length.
    pub size: usize,
}

impl FieldSpan {
    /// One past the last byte this field covers.
    #[must_use]
    pub const fn end(&self) -> u64 {
        self.offset.saturating_add(self.size as u64)
    }

    /// Does this field cover `offset`?
    #[must_use]
    pub const fn covers_offset(&self, offset: u64) -> bool {
        offset >= self.offset && offset < self.end()
    }
}

impl Extent for FieldSpan {
    fn first_byte(&self) -> u64 {
        self.offset
    }

    fn byte_len(&self) -> usize {
        self.size
    }
}

/// One field, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldReading {
    /// The field's name.
    pub name: String,
    /// Where it starts, as an index into the buffer [`applied`] was given,
    /// which is the absolute file offset whenever that buffer starts at the
    /// file's first byte.
    pub offset: u64,
    /// How many bytes it actually covers.
    ///
    /// The declared width, except for a field the file ends in the middle of,
    /// where it is the part that exists. A renderer colouring these bytes
    /// therefore never colours a byte the file does not have.
    pub size: usize,
    /// The decoded value, ready to print.
    pub value: String,
}

impl FieldReading {
    /// One past the last byte this reading covers.
    #[must_use]
    pub const fn end(&self) -> u64 {
        self.offset.saturating_add(self.size as u64)
    }

    /// Does this reading cover `offset`?
    #[must_use]
    pub const fn covers(&self, offset: u64) -> bool {
        offset >= self.offset && offset < self.end()
    }
}

impl Extent for FieldReading {
    fn first_byte(&self) -> u64 {
        self.offset
    }

    fn byte_len(&self) -> usize {
        self.size
    }
}

#[path = "template_parse.rs"]
mod parse;

#[path = "template_read.rs"]
mod read;

pub use read::{applied, coverage, extents, field_at, load_dir, matches};

#[cfg(test)]
#[path = "template_tests.rs"]
mod tests;
