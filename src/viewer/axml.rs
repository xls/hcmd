//! Android Binary XML: the compiled form an `.apk` carries its manifest in.
//!
//! A `.xml` inside an APK is not text. It is a chunked binary format - a pool
//! of every string in the document, then a tree of tags addressing that pool by
//! index - and opening one shows a dump, because a dump is what it is. This
//! turns it back into the document it was compiled from, so the viewer has text
//! to show and the highlighter has XML to colour.
//!
//! Read-only and pure: bytes in, a string out, nothing on the filesystem and no
//! new dependency. Every read is bounds-checked and answers `None` rather than
//! panicking, because the input is a file from somewhere else and a viewer that
//! can be crashed by a malformed one is a viewer that can be crashed.

/// The chunk types this understands. Everything else is skipped by its own
/// recorded size, which is how the format is meant to be read.
mod chunk {
    /// The document itself.
    pub const XML: u16 = 0x0003;
    /// The pool every name and value is an index into.
    pub const STRING_POOL: u16 = 0x0001;
    /// `<tag ...>`
    pub const START_ELEMENT: u16 = 0x0102;
    /// `</tag>`
    pub const END_ELEMENT: u16 = 0x0103;
    /// A namespace coming into scope.
    pub const START_NAMESPACE: u16 = 0x0100;
    /// Text between tags.
    pub const CDATA: u16 = 0x0104;
}

/// How an attribute's value should be read.
mod value {
    /// An index into the string pool.
    pub const STRING: u8 = 0x03;
    /// A resource id, which this file cannot resolve to a name.
    pub const REFERENCE: u8 = 0x01;
    /// An attribute id, likewise.
    pub const ATTRIBUTE: u8 = 0x02;
    /// IEEE 754, in the same four bytes.
    pub const FLOAT: u8 = 0x04;
    /// Decimal.
    pub const INT_DEC: u8 = 0x10;
    /// Hexadecimal, and written back as hexadecimal.
    pub const INT_HEX: u8 = 0x11;
    /// Zero or not.
    pub const INT_BOOLEAN: u8 = 0x12;
}

/// Is this Android Binary XML?
///
/// The magic alone is four bytes that a text file could begin with by accident,
/// so the length the header records must also be the length the file actually
/// is. Together they are as close to certain as a sniff gets - and the name is
/// no help at all here, because the file is called `.xml` either way.
#[must_use]
pub fn looks_like_axml(bytes: &[u8]) -> bool {
    let Some((kind, header, size)) = chunk_header(bytes, 0) else {
        return false;
    };
    kind == chunk::XML && header == 8 && size as usize == bytes.len()
}

/// The document length this file's header records, when the magic is there.
///
/// The sniff needs both halves and only one of them is in the first bytes, so
/// a caller with the file's real length compares them itself.
#[must_use]
pub fn recorded_len(bytes: &[u8]) -> Option<u32> {
    let (kind, header, size) = chunk_header(bytes, 0)?;
    (kind == chunk::XML && header == 8).then_some(size)
}

/// A chunk's `(type, header size, total size)` at `at`.
fn chunk_header(bytes: &[u8], at: usize) -> Option<(u16, u16, u32)> {
    Some((
        u16_at(bytes, at)?,
        u16_at(bytes, at + 2)?,
        u32_at(bytes, at + 4)?,
    ))
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    let raw = bytes.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([*raw.first()?, *raw.get(1)?]))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let raw = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([
        *raw.first()?,
        *raw.get(1)?,
        *raw.get(2)?,
        *raw.get(3)?,
    ]))
}

/// Every string in the document, by the index the tree addresses it with.
struct Pool {
    strings: Vec<String>,
}

impl Pool {
    /// `None` for the index the format uses to mean "no string", which is how
    /// an unprefixed attribute says it has no namespace.
    fn get(&self, index: u32) -> Option<&str> {
        if index == u32::MAX {
            return None;
        }
        self.strings.get(index as usize).map(String::as_str)
    }

    /// The string at `index`, or a placeholder naming the index that was not
    /// there. A document that addresses a string it does not contain is
    /// damaged, and saying so beats both a panic and a silent blank.
    fn name(&self, index: u32) -> String {
        self.get(index)
            .map_or_else(|| format!("<string {index}>"), str::to_string)
    }
}

/// Read the pool chunk beginning at `at`.
fn read_pool(bytes: &[u8], at: usize) -> Option<Pool> {
    let (kind, header_size, _) = chunk_header(bytes, at)?;
    // Checked rather than assumed: the pool is the first chunk in every
    // document the tools emit, and a file where it is not is one this should
    // decline instead of reading whatever is there as offsets.
    if kind != chunk::STRING_POOL {
        return None;
    }
    let count = u32_at(bytes, at + 8)? as usize;
    let flags = u32_at(bytes, at + 16)?;
    let data_start = at.checked_add(u32_at(bytes, at + 20)? as usize)?;
    // Bit 8 says the strings are UTF-8; without it they are UTF-16, which is
    // what a manifest built by the platform tools carries.
    let utf8 = flags & (1 << 8) != 0;
    let offsets = at.checked_add(header_size as usize)?;

    let mut strings = Vec::with_capacity(count.min(4096));
    for i in 0..count {
        let Some(offset) = u32_at(bytes, offsets.checked_add(i.checked_mul(4)?)?) else {
            break;
        };
        let Some(start) = data_start.checked_add(offset as usize) else {
            break;
        };
        let text = if utf8 {
            read_utf8(bytes, start)
        } else {
            read_utf16(bytes, start)
        };
        strings.push(text.unwrap_or_default());
    }
    Some(Pool { strings })
}

/// A UTF-16 pool string: a length in code units, then the units themselves.
///
/// A length with the top bit set is the high half of a longer one, which is
/// how the format expresses a string too long to count in fifteen bits.
fn read_utf16(bytes: &[u8], at: usize) -> Option<String> {
    let first = u16_at(bytes, at)?;
    let (len, start) = if first & 0x8000 == 0 {
        (first as usize, at.checked_add(2)?)
    } else {
        let low = u16_at(bytes, at.checked_add(2)?)?;
        (
            ((usize::from(first & 0x7fff)) << 16) | usize::from(low),
            at.checked_add(4)?,
        )
    };
    let mut units = Vec::with_capacity(len.min(4096));
    for i in 0..len {
        units.push(u16_at(bytes, start.checked_add(i.checked_mul(2)?)?)?);
    }
    Some(String::from_utf16_lossy(&units))
}

/// A UTF-8 pool string: the length in UTF-16 units, then in bytes, then the
/// bytes. Each length is one byte unless its top bit is set, and the UTF-16
/// count is not used here - the bytes are what is wanted.
fn read_utf8(bytes: &[u8], at: usize) -> Option<String> {
    let mut cursor = at;
    for _ in 0..2 {
        let first = *bytes.get(cursor)?;
        cursor = cursor.checked_add(if first & 0x80 == 0 { 1 } else { 2 })?;
    }
    // The byte length is the second of the two, re-read now that its width is
    // known.
    let first = *bytes.get(at)?;
    let len_at = at.checked_add(if first & 0x80 == 0 { 1 } else { 2 })?;
    let len_first = *bytes.get(len_at)?;
    let len = if len_first & 0x80 == 0 {
        usize::from(len_first)
    } else {
        (usize::from(len_first & 0x7f) << 8) | usize::from(*bytes.get(len_at.checked_add(1)?)?)
    };
    let raw = bytes.get(cursor..cursor.checked_add(len)?)?;
    Some(String::from_utf8_lossy(raw).into_owned())
}

/// One attribute's value, written the way it would have been in the source.
fn format_value(pool: &Pool, kind: u8, data: u32) -> String {
    match kind {
        value::STRING => pool.name(data),
        value::INT_BOOLEAN => if data == 0 { "false" } else { "true" }.to_string(),
        // Signed: a manifest carries `-1` for a good many things.
        value::INT_DEC => (data as i32).to_string(),
        value::INT_HEX => format!("0x{data:x}"),
        value::FLOAT => f32::from_bits(data).to_string(),
        // Resolving one of these needs the resource table from the APK, which
        // is a different file; the id is the honest answer here.
        value::REFERENCE => format!("@0x{data:08x}"),
        value::ATTRIBUTE => format!("?0x{data:08x}"),
        _ => format!("0x{data:08x}"),
    }
}

/// Escape the five characters that cannot appear as themselves in XML text.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// The prefix a namespace URI is written with, and the declarations to put on
/// the root element.
///
/// A document usually declares its own, but it is not obliged to and a compiled
/// manifest often carries none at all - the sample this was written against has
/// no namespace chunk and every `android:` attribute naming its URI directly.
/// So the URIs actually used are collected first and given prefixes: the
/// Android one by its conventional name, anything else by the last word of its
/// URI, and a bare number only when that fails. Without this the attributes
/// come out unprefixed, which is not what the source said and not valid against
/// the schema either.
fn namespaces(bytes: &[u8], pool: &Pool) -> Vec<(u32, String)> {
    const ANDROID: &str = "http://schemas.android.com/apk/res/android";
    let mut out: Vec<(u32, String)> = Vec::new();
    let mut at = 8usize;
    while let Some((kind, _, size)) = chunk_header(bytes, at) {
        let size = size as usize;
        if size == 0 {
            break;
        }
        // A declared prefix is the document's own word and always wins.
        if kind == chunk::START_NAMESPACE
            && let (Some(prefix), Some(uri)) = (u32_at(bytes, at + 16), u32_at(bytes, at + 20))
            && let Some(prefix) = pool.get(prefix)
            && !out.iter().any(|(known, _)| *known == uri)
        {
            out.push((uri, prefix.to_string()));
        }
        if kind == chunk::START_ELEMENT {
            for (ns, _) in attributes(bytes, at) {
                if ns == u32::MAX || out.iter().any(|(known, _)| *known == ns) {
                    continue;
                }
                let uri = pool.name(ns);
                let name = if uri == ANDROID {
                    "android".to_string()
                } else {
                    uri.rsplit(['/', ':'])
                        .find(|part| !part.is_empty() && part.chars().all(char::is_alphanumeric))
                        .map_or_else(
                            || format!("ns{}", out.len().saturating_add(1)),
                            str::to_string,
                        )
                };
                out.push((ns, name));
            }
        }
        let Some(next) = at.checked_add(size) else {
            break;
        };
        at = next;
    }
    out
}

/// The `(namespace, offset)` of each attribute on the element at `at`.
fn attributes(bytes: &[u8], at: usize) -> Vec<(u32, usize)> {
    let (Some(start), Some(size), Some(count)) = (
        u16_at(bytes, at + 24),
        u16_at(bytes, at + 26),
        u16_at(bytes, at + 28),
    ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for i in 0..count {
        let Some(entry) = usize::from(i)
            .checked_mul(usize::from(size))
            .and_then(|off| {
                at.checked_add(16)?
                    .checked_add(usize::from(start))?
                    .checked_add(off)
            })
        else {
            break;
        };
        let Some(ns) = u32_at(bytes, entry) else {
            break;
        };
        out.push((ns, entry));
    }
    out
}

/// Turn an Android Binary XML document back into readable XML.
///
/// `None` when the bytes are not one, or when the document is damaged in a way
/// that leaves nothing worth showing.
#[must_use]
pub fn decode(bytes: &[u8]) -> Option<String> {
    if !looks_like_axml(bytes) {
        return None;
    }
    // The pool is the first chunk, and everything else addresses it - so it is
    // read before the walk rather than during it, which is also what lets the
    // namespaces be known before the root element is written.
    let pool = read_pool(bytes, 8)?;
    let prefixes = namespaces(bytes, &pool);
    let mut at = 8usize;
    let mut root_written = false;
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    let mut depth = 0usize;

    while let Some((kind, _, size)) = chunk_header(bytes, at) {
        let size = size as usize;
        if size == 0 {
            break;
        }
        match kind {
            chunk::START_ELEMENT => {
                // The declarations belong on the root, which is the first
                // element there is.
                let declare = (!root_written).then_some(prefixes.as_slice());
                write_element(&mut out, bytes, at, &pool, &prefixes, depth, declare);
                root_written = true;
                depth = depth.saturating_add(1);
            }
            chunk::END_ELEMENT => {
                depth = depth.saturating_sub(1);
                if let Some(name) = u32_at(bytes, at + 20) {
                    out.push_str(&"  ".repeat(depth));
                    out.push_str("</");
                    out.push_str(&pool.name(name));
                    out.push_str(">\n");
                }
            }
            chunk::CDATA => {
                if let Some(text) = u32_at(bytes, at + 16) {
                    let text = pool.name(text);
                    if !text.trim().is_empty() {
                        out.push_str(&"  ".repeat(depth));
                        out.push_str(&escape(text.trim()));
                        out.push('\n');
                    }
                }
            }
            // Anything else - the resource map, a namespace going out of scope
            // - contributes no text and is stepped over by its own size.
            _ => {}
        }
        at = at.checked_add(size)?;
    }
    (depth == 0 && out.lines().count() > 1).then_some(out)
}

/// One `<tag ...>` line, with its attributes.
fn write_element(
    out: &mut String,
    bytes: &[u8],
    at: usize,
    pool: &Pool,
    prefixes: &[(u32, String)],
    depth: usize,
    declare: Option<&[(u32, String)]>,
) {
    let Some(name) = u32_at(bytes, at + 20) else {
        return;
    };
    out.push_str(&"  ".repeat(depth));
    out.push('<');
    out.push_str(&pool.name(name));

    for (uri, prefix) in declare.unwrap_or(&[]) {
        out.push('\n');
        out.push_str(&"  ".repeat(depth.saturating_add(1)));
        out.push_str(&format!("xmlns:{prefix}=\"{}\"", escape(&pool.name(*uri))));
    }
    for (ns, entry) in attributes(bytes, at) {
        let (Some(attr), Some(raw), Some(kind), Some(data)) = (
            u32_at(bytes, entry + 4),
            u32_at(bytes, entry + 8),
            bytes.get(entry + 15).copied(),
            u32_at(bytes, entry + 16),
        ) else {
            break;
        };
        // The raw string is what was written in the source, when the compiler
        // kept it; the typed value is what it became. The first is friendlier
        // to read, so it wins where it exists.
        let text = pool
            .get(raw)
            .map_or_else(|| format_value(pool, kind, data), str::to_string);
        let prefix = prefixes
            .iter()
            .find(|(uri, _)| *uri == ns)
            .map(|(_, name)| name.as_str());
        out.push('\n');
        out.push_str(&"  ".repeat(depth.saturating_add(1)));
        if let Some(prefix) = prefix {
            out.push_str(prefix);
            out.push(':');
        }
        out.push_str(&pool.name(attr));
        out.push_str("=\"");
        out.push_str(&escape(&text));
        out.push('"');
    }
    out.push_str(">\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file header for a document `len` bytes long.
    fn header(len: u32) -> Vec<u8> {
        let mut out = vec![0x03, 0x00, 0x08, 0x00];
        out.extend_from_slice(&len.to_le_bytes());
        out
    }

    #[test]
    fn it_is_recognised_by_its_shape_and_not_by_its_name() {
        // The magic and the recorded length together. Either alone is a guess:
        // the name says `.xml` for both a compiled manifest and a text one.
        let mut doc = header(12);
        doc.extend_from_slice(&[0, 0, 0, 0]);
        assert!(looks_like_axml(&doc), "magic, and the length agrees");

        let mut lying = header(999);
        lying.extend_from_slice(&[0, 0, 0, 0]);
        assert!(!looks_like_axml(&lying), "a length that is not the file's");

        assert!(!looks_like_axml(b"<?xml version=\"1.0\"?><root/>"), "text");
        assert!(!looks_like_axml(b""), "nothing at all");
        assert!(!looks_like_axml(&[0x03, 0x00]), "a truncated header");
    }

    #[test]
    fn a_pool_string_is_read_at_either_width() {
        // UTF-16: a length in code units, then the units.
        let utf16 = [3u8, 0, b'a', 0, b'b', 0, b'c', 0, 0, 0];
        assert_eq!(read_utf16(&utf16, 0).as_deref(), Some("abc"));

        // UTF-8: the UTF-16 length, then the byte length, then the bytes.
        let utf8 = [3u8, 3, b'x', b'y', b'z', 0];
        assert_eq!(read_utf8(&utf8, 0).as_deref(), Some("xyz"));

        // A length that runs off the end answers nothing rather than panicking:
        // the input is a file from somewhere else.
        assert_eq!(read_utf16(&[9u8, 0, b'a', 0], 0), None);
        assert_eq!(read_utf8(&[9u8, 9, b'a'], 0), None);
    }

    #[test]
    fn a_value_is_written_the_way_it_was_declared() {
        let pool = Pool {
            strings: vec!["chrome".to_string()],
        };
        assert_eq!(format_value(&pool, value::STRING, 0), "chrome");
        assert_eq!(format_value(&pool, value::INT_BOOLEAN, 0), "false");
        assert_eq!(format_value(&pool, value::INT_BOOLEAN, 0xffff_ffff), "true");
        assert_eq!(format_value(&pool, value::INT_DEC, 36), "36");
        // Signed: a manifest carries -1 for a good many things.
        assert_eq!(format_value(&pool, value::INT_DEC, 0xffff_ffff), "-1");
        assert_eq!(format_value(&pool, value::INT_HEX, 0x1f), "0x1f");
        // A reference needs the APK's resource table, which is another file.
        assert_eq!(format_value(&pool, value::REFERENCE, 0x7f01), "@0x00007f01");
    }

    #[test]
    fn the_five_characters_xml_reserves_are_escaped() {
        assert_eq!(escape("a&b<c>d\"e'f"), "a&amp;b&lt;c&gt;d&quot;e&apos;f");
    }

    /// A real manifest out of a real APK, when the maintainer's sample is
    /// there: the only input that proves this against the format as the
    /// platform tools actually emit it.
    fn sample() -> Option<Vec<u8>> {
        let home = std::env::var("HOME").ok()?;
        std::fs::read(std::path::Path::new(&home).join("TestData/AndroidManifest.xml")).ok()
    }

    #[test]
    fn a_real_manifest_decodes_to_the_document_it_was_compiled_from() {
        let Some(bytes) = sample() else {
            eprintln!("SKIPPING: no sample manifest at ~/TestData");
            return;
        };
        assert!(looks_like_axml(&bytes));
        let text = decode(&bytes).expect("it decodes");

        // The namespace this file never declared: it carries the URI on every
        // attribute and no namespace chunk at all, so the prefix is recovered
        // rather than read.
        assert!(
            text.contains("xmlns:android=\"http://schemas.android.com/apk/res/android\""),
            "the root declares the namespace it uses"
        );
        assert!(
            text.contains("android:versionName=\"136.0.7103.60\""),
            "a prefixed attribute, at its source value"
        );
        assert!(
            text.contains("package=\"com.android.chrome\""),
            "and an unprefixed one stays unprefixed"
        );
        assert!(text.starts_with("<?xml "), "it opens as a document");
        assert!(text.contains("<manifest"), "with the tag it is");

        // Every tag that opened is closed: the walk balanced, which is what
        // `decode` refuses to return without.
        let opens = text.matches('<').count() - text.matches("</").count();
        let closes = text.matches("</").count();
        assert!(opens > 0 && closes > 0, "there is a tree here");
    }

    #[test]
    fn text_and_rubbish_are_left_alone() {
        assert_eq!(decode(b"<?xml version=\"1.0\"?><root/>"), None);
        assert_eq!(decode(&[0xff; 64]), None);
        assert_eq!(decode(b""), None);
    }
}
