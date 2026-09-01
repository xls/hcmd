//! The encoding model.
//!
//! > `encoding_rs` decodes; every encoding it supports is selectable.
//! > `viewer.encoding.default = "auto"` sniffs with `chardetng` over the first
//! > chunk. A BOM wins over the sniff when `bom = true`. … Decoding is
//! > incremental and window-local; changing encoding re-decodes the visible
//! > window only, not the file. Invalid sequences render as a replacement glyph
//! > rather than failing the open.
//!
//! # The hard part, and how it is solved
//!
//! A window boundary can fall in the middle of a multi-byte sequence, and a
//! stateful decoder cannot simply be restarted at an arbitrary offset. Both are
//! true. The answer is not to make the decoder resumable - it is to **stop
//! putting window boundaries in the middle of sequences**:
//!
//! 1. In text mode the viewer's window always begins at a **line start**, and a
//!    line start is a clean restart point in every encoding here. None of them
//!    can produce `\n` from a byte that is part of a multi-byte sequence: UTF-8
//!    keeps continuation bytes ≥ 0x80, every DBCS the WHATWG set carries
//!    (Shift_JIS, EUC-JP, EUC-KR, GBK/GB18030, Big5) keeps trail bytes ≥ 0x40,
//!    ISO-2022-JP's escape sequences contain no `\n`, and UTF-16's line breaks
//!    are found on the code-unit grid rather than the byte grid - which is what
//!    [`LineTerm`] is for. So the decoder is created **fresh for each line**,
//!    used, and dropped. There is no cross-window state to restart, which is
//!    also exactly why changing the encoding costs one window and not one file.
//! 2. In hex mode nothing is decoded at all. Bytes are bytes, and the ASCII
//!    gutter is byte-wise by definition.
//! 3. What is left is the residue: a window that genuinely *cannot* be snapped
//!    to a line start - a line longer than the window, or a forward-only source
//!    whose index has not reached here yet. [`resync`] handles that case
//!    locally and honestly, and [`Resync::LeadIn`] is where it admits it cannot
//!    and says the first character may be wrong.
//!
//! # `cp437`
//!
//! the design names `cp437` in the `F8` shortlist. `encoding_rs` implements
//! the WHATWG Encoding Standard, which deliberately excludes the IBM DOS code
//! pages, so `cp437` is not one of its labels. It is carried here as a
//! [`CodePage`] - a 256-entry table - because a file manager that cannot read a
//! DOS text file is missing something a file manager is for. That is the only
//! reason [`TextEncoding`] is an enum rather than an `&'static Encoding`.

use encoding_rs::Encoding;

use crate::config::ViewerEncodingConfig;

/// How much of the file `chardetng` is shown ("the first
/// chunk").
pub const SNIFF_BYTES: usize = 64 * 1024;

/// How much of the file the binary test looks at.
pub const BINARY_PROBE_BYTES: usize = 8 * 1024;

/// A single-byte code page `encoding_rs` does not carry. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodePage {
    /// The label the config file and the status line use.
    pub label: &'static str,
    /// What each of the 256 byte values decodes to.
    pub table: &'static [char; 256],
}

/// IBM code page 437 - the original PC character set, box drawing included.
pub static CP437: CodePage = CodePage {
    label: "cp437",
    table: &CP437_TABLE,
};

/// The 0x00-0xFF mapping of [`CP437`].
///
/// 0x00-0x1F are the *control* range rather than CP437's glyphs for them: a
/// viewer that rendered byte 0x0A as `◙` could not show a line break, and the
/// text-mode renderer needs the control meanings. Hex mode shows the bytes.
static CP437_TABLE: [char; 256] = {
    let mut t = ['\u{0}'; 256];
    let mut i = 0;
    while i < 128 {
        // Safe by construction: every value below 128 is a scalar value.
        t[i] = match char::from_u32(i as u32) {
            Some(c) => c,
            None => '\u{fffd}',
        };
        i += 1;
    }
    let high = [
        'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', 'É', 'æ',
        'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', 'á', 'í', 'ó', 'ú',
        'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', '░', '▒', '▓', '│', '┤', '╡',
        '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐', '└', '┴', '┬', '├', '─', '┼', '╞', '╟',
        '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧', '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘',
        '┌', '█', '▄', '▌', '▐', '▀', 'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ',
        '∞', 'φ', 'ε', '∩', '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²',
        '■', '\u{a0}',
    ];
    let mut j = 0;
    while j < 128 {
        t[128 + j] = high[j];
        j += 1;
    }
    t
};

/// Every code page carried here rather than by `encoding_rs`.
pub static CODE_PAGES: &[&CodePage] = &[&CP437];

/// An encoding the viewer can decode with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextEncoding {
    /// One of the WHATWG encodings `encoding_rs` implements.
    Rs(&'static Encoding),
    /// A single-byte code page carried here. See the module docs.
    Page(&'static CodePage),
}

impl TextEncoding {
    /// UTF-8. The default when nothing else has an opinion.
    pub const UTF8: Self = Self::Rs(encoding_rs::UTF_8);

    /// The label shown in the status line and written in `config.toml`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Rs(enc) => enc.name(),
            Self::Page(page) => page.label,
        }
    }

    /// How a decoder can be restarted at an arbitrary byte offset.
    pub fn resync(self) -> Resync {
        match self {
            Self::Page(_) => Resync::Any,
            Self::Rs(enc) => {
                if enc == encoding_rs::UTF_8 {
                    Resync::Utf8
                } else if enc == encoding_rs::UTF_16LE || enc == encoding_rs::UTF_16BE {
                    Resync::Unit(2)
                } else if enc.is_single_byte() {
                    Resync::Any
                } else {
                    Resync::LeadIn
                }
            }
        }
    }

    /// Where this encoding's line breaks live: on the byte grid, or on a
    /// two-byte code-unit grid.
    pub fn line_term(self) -> LineTerm {
        match self {
            Self::Rs(enc) if enc == encoding_rs::UTF_16LE => LineTerm::Utf16Le,
            Self::Rs(enc) if enc == encoding_rs::UTF_16BE => LineTerm::Utf16Be,
            _ => LineTerm::Byte,
        }
    }

    /// Decode one line's bytes.
    ///
    /// **Never fails.** A malformed sequence becomes U+FFFD and the second
    /// element of the pair says it happened, so the status line can mention it
    /// without the open having been refused.
    ///
    /// Stateless by construction: one call, one fresh decoder, no state kept.
    /// That is what makes "changing the encoding re-decodes the visible window
    /// only" true rather than aspirational.
    pub fn decode(self, bytes: &[u8]) -> (String, bool) {
        match self {
            Self::Rs(enc) => {
                let (text, _, had_errors) = enc.decode(bytes);
                (text.into_owned(), had_errors)
            }
            Self::Page(page) => {
                let text: String = bytes
                    .iter()
                    .map(|b| page.table.get(*b as usize).copied().unwrap_or('\u{fffd}'))
                    .collect();
                (text, false)
            }
        }
    }
}

/// The label a user or a config file wrote, resolved to an encoding.
///
/// `encoding_rs`'s own label table first - so every WHATWG alias
/// (`latin1`, `cp1252`, `iso-8859-1`, …) works - then the code pages carried
/// here. `None` for a label nothing recognises, which the caller reports and
/// falls back from rather than failing the open.
pub fn encoding_for(label: &str) -> Option<TextEncoding> {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(page) = CODE_PAGES
        .iter()
        .find(|p| p.label.eq_ignore_ascii_case(trimmed))
    {
        return Some(TextEncoding::Page(page));
    }
    Encoding::for_label(trimmed.as_bytes()).map(TextEncoding::Rs)
}

/// How a decoder can be restarted at an arbitrary byte offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resync {
    /// Every byte offset is a character boundary: all single-byte encodings.
    Any,
    /// UTF-8. Step forward past continuation bytes; at most three.
    Utf8,
    /// Fixed-width code units. Round down to a multiple of `n` measured from
    /// the start of the file, which is exact rather than a guess.
    Unit(u8),
    /// No local rule exists. The caller decodes with a bounded lead-in and
    /// accepts that the first character may be a replacement glyph.
    LeadIn,
}

impl Resync {
    /// How many bytes of lead-in to read before the wanted offset when there is
    /// no local rule. One kilobyte re-synchronises every variable-width
    /// encoding here many times over, and it is bounded, which is the property
    /// that matters.
    pub const LEAD_IN: u64 = 1024;
}

/// The clean start offset at or before `want`, and whether it is a guess.
///
/// `bytes` are the file's bytes starting at `bytes_at`; they must cover `want`.
/// The returned offset is never above `want` and never below
/// `want - Resync::LEAD_IN`, so the caller's window stays bounded.
///
/// The `bool` is the honesty rule applied to decoding: `true` means
/// the first character of the decoded run may be wrong, and the status line
/// should say the window is approximate rather than pretend.
pub fn resync(enc: TextEncoding, bytes_at: u64, bytes: &[u8], want: u64) -> (u64, bool) {
    match enc.resync() {
        Resync::Any => (want, false),
        Resync::Unit(n) => {
            let n = u64::from(n.max(1));
            (want.saturating_sub(want % n), false)
        }
        Resync::Utf8 => {
            // Walk *back* to the lead byte of the sequence `want` is inside.
            // At most three steps, because a UTF-8 sequence is at most four
            // bytes long.
            let mut at = want;
            for _ in 0..3 {
                let idx = match usize::try_from(at.saturating_sub(bytes_at)) {
                    Ok(i) => i,
                    Err(_) => return (want, false),
                };
                match bytes.get(idx) {
                    // A continuation byte: the sequence started earlier.
                    Some(b) if b & 0xC0 == 0x80 && at > bytes_at => at = at.saturating_sub(1),
                    _ => return (at, false),
                }
            }
            (at, false)
        }
        Resync::LeadIn => (want.saturating_sub(Resync::LEAD_IN).max(bytes_at), true),
    }
}

/// Where a line break lives for an encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineTerm {
    /// A single `0x0A` byte. Every ASCII-compatible encoding, which is all of
    /// them except UTF-16.
    Byte,
    /// `0A 00` on the even-offset code-unit grid.
    Utf16Le,
    /// `00 0A` on the even-offset code-unit grid.
    Utf16Be,
}

impl LineTerm {
    /// How many bytes one code unit takes.
    pub const fn unit(self) -> usize {
        match self {
            Self::Byte => 1,
            Self::Utf16Le | Self::Utf16Be => 2,
        }
    }

    /// The offset *within* `bytes` of the first line break at or after `from`,
    /// or `None` when there is none.
    ///
    /// The returned offset is the break's first byte; the next line starts
    /// `self.unit()` bytes later.
    pub fn find(self, bytes: &[u8], from: usize) -> Option<usize> {
        match self {
            Self::Byte => bytes
                .get(from..)
                .and_then(|s| s.iter().position(|b| *b == b'\n'))
                .map(|i| i.saturating_add(from)),
            Self::Utf16Le | Self::Utf16Be => {
                let (lo, hi) = if matches!(self, Self::Utf16Le) {
                    (b'\n', 0)
                } else {
                    (0, b'\n')
                };
                // Only even offsets are code-unit starts, which is precisely
                // what stops `0x0A` inside another unit being read as a break.
                let start = from.saturating_add(from % 2);
                let mut i = start;
                while i.saturating_add(1) < bytes.len() {
                    if bytes.get(i) == Some(&lo) && bytes.get(i.saturating_add(1)) == Some(&hi) {
                        return Some(i);
                    }
                    i = i.saturating_add(2);
                }
                None
            }
        }
    }

    /// The offset within `bytes` of the last line break strictly before
    /// `before`, or `None`.
    pub fn rfind(self, bytes: &[u8], before: usize) -> Option<usize> {
        let end = before.min(bytes.len());
        match self {
            Self::Byte => bytes
                .get(..end)
                .and_then(|s| s.iter().rposition(|b| *b == b'\n')),
            Self::Utf16Le | Self::Utf16Be => {
                let (lo, hi) = if matches!(self, Self::Utf16Le) {
                    (b'\n', 0)
                } else {
                    (0, b'\n')
                };
                let mut i = end.saturating_sub(end % 2);
                while i >= 2 {
                    i = i.saturating_sub(2);
                    if bytes.get(i) == Some(&lo) && bytes.get(i.saturating_add(1)) == Some(&hi) {
                        return Some(i);
                    }
                }
                None
            }
        }
    }

    /// Strip the trailing line break, and the `\r` of a CRLF with it.
    pub fn trim_break(self, bytes: &[u8]) -> &[u8] {
        match self {
            Self::Byte => {
                let b = bytes.strip_suffix(b"\n").unwrap_or(bytes);
                b.strip_suffix(b"\r").unwrap_or(b)
            }
            Self::Utf16Le => {
                let b = bytes.strip_suffix(b"\n\0").unwrap_or(bytes);
                b.strip_suffix(b"\r\0").unwrap_or(b)
            }
            Self::Utf16Be => {
                let b = bytes.strip_suffix(b"\0\n").unwrap_or(bytes);
                b.strip_suffix(b"\0\r").unwrap_or(b)
            }
        }
    }
}

/// Where the active encoding came from, for the status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detected {
    /// A byte-order mark said so, and `viewer.encoding.bom` is on.
    Bom,
    /// `chardetng` sniffed it.
    Sniffed,
    /// `viewer.encoding.default` named it.
    Configured,
    /// `viewer.encoding.fallback`, because nothing else resolved.
    Fallback,
    /// The user chose it with `F8`.
    Chosen,
}

impl Detected {
    /// A short word for the status line.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Bom => "bom",
            Self::Sniffed => "auto",
            Self::Configured => "config",
            Self::Fallback => "fallback",
            Self::Chosen => "chosen",
        }
    }
}

/// The result of resolving `[viewer.encoding]` against a file's first chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Encodings {
    /// What to decode with.
    pub encoding: TextEncoding,
    /// How that was arrived at.
    pub how: Detected,
    /// How many bytes of BOM to skip at the start of the file. Zero when there
    /// is none, or when `viewer.encoding.bom` is off.
    pub bom_len: u64,
}

/// The byte-order mark at the head of `prefix`, if any.
pub fn bom(prefix: &[u8]) -> Option<(TextEncoding, u64)> {
    if prefix.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Some((TextEncoding::Rs(encoding_rs::UTF_8), 3));
    }
    if prefix.starts_with(&[0xFF, 0xFE]) {
        return Some((TextEncoding::Rs(encoding_rs::UTF_16LE), 2));
    }
    if prefix.starts_with(&[0xFE, 0xFF]) {
        return Some((TextEncoding::Rs(encoding_rs::UTF_16BE), 2));
    }
    None
}

/// Resolve `[viewer.encoding]` against the first chunk of a file.
///
///
/// Order: a BOM (when `bom = true`), then `default` if it names an encoding,
/// then the `chardetng` sniff if `default = "auto"` and `detect = true`, then
/// `fallback`, then UTF-8. Nothing here can fail - an unrecognised label falls
/// through to the next step rather than refusing to open the file.
pub fn resolve(cfg: &ViewerEncodingConfig, prefix: &[u8], complete: bool) -> Encodings {
    if cfg.bom
        && let Some((encoding, bom_len)) = bom(prefix)
    {
        return Encodings {
            encoding,
            how: Detected::Bom,
            bom_len,
        };
    }
    let auto = cfg.default.trim().eq_ignore_ascii_case("auto");
    if !auto && let Some(encoding) = encoding_for(&cfg.default) {
        return Encodings {
            encoding,
            how: Detected::Configured,
            bom_len: 0,
        };
    }
    if auto && cfg.detect {
        // UTF-16 with no byte order mark, asked before `chardetng` because
        // `chardetng` does not answer it: its detector is for legacy
        // single-byte encodings and a UTF-16 file comes back from it as one of
        // those. Left to that, every other byte is a NUL, the binary test sees
        // them and the file opens in hex - which is what a Windows-written
        // `.xml` did.
        if let Some(encoding) = sniff_utf16_without_bom(prefix) {
            return Encodings {
                encoding,
                how: Detected::Sniffed,
                bom_len: 0,
            };
        }
        return Encodings {
            encoding: sniff(prefix, complete),
            how: Detected::Sniffed,
            bom_len: 0,
        };
    }
    match encoding_for(&cfg.fallback) {
        Some(encoding) => Encodings {
            encoding,
            how: Detected::Fallback,
            bom_len: 0,
        },
        None => Encodings {
            encoding: TextEncoding::UTF8,
            how: Detected::Fallback,
            bom_len: 0,
        },
    }
}

/// UTF-16 recognised by its shape rather than by a mark it does not carry.
///
/// Text that is ASCII underneath - which markup, source and configuration all
/// are - becomes, in UTF-16, one ASCII byte and one NUL for every character,
/// and which of the pair is the NUL says which way round it is. So: the NULs
/// must be almost all on one side, almost none on the other, and what is left
/// must read as text. That last condition is what keeps a genuinely binary
/// file - which has NULs in no pattern at all and arbitrary bytes between them
/// - from being mistaken for a document.
///
/// `None` when the shape is not there, which is every other kind of file.
fn sniff_utf16_without_bom(prefix: &[u8]) -> Option<TextEncoding> {
    // Enough pairs to mean something. A handful of bytes can look like
    // anything, and guessing from them would be worse than not guessing.
    const MIN_PAIRS: usize = 8;
    let usable = prefix.len() & !1;
    let pairs = usable / 2;
    if pairs < MIN_PAIRS {
        return None;
    }
    let body = prefix.get(..usable)?;
    let (mut even_nul, mut odd_nul, mut printable) = (0usize, 0usize, 0usize);
    for (i, byte) in body.iter().enumerate() {
        if *byte == 0 {
            if i % 2 == 0 {
                even_nul = even_nul.saturating_add(1);
            } else {
                odd_nul = odd_nul.saturating_add(1);
            }
        } else if *byte >= 0x20 || matches!(*byte, b'\n' | b'\r' | b'\t') {
            printable = printable.saturating_add(1);
        }
    }
    // Nine in ten of one side NUL, at most one in ten of the other, and what
    // is not NUL is text.
    let nearly_all = pairs.saturating_mul(9) / 10;
    let hardly_any = pairs / 10;
    let readable = printable >= nearly_all;
    if odd_nul >= nearly_all && even_nul <= hardly_any && readable {
        return encoding_for("utf-16le");
    }
    if even_nul >= nearly_all && odd_nul <= hardly_any && readable {
        return encoding_for("utf-16be");
    }
    None
}

/// Sniff an encoding from the first chunk with `chardetng`.
///
/// `complete` says whether `prefix` is the whole file, which is the hint
/// `chardetng` uses to decide how confident it may be.
pub fn sniff(prefix: &[u8], complete: bool) -> TextEncoding {
    // Valid UTF-8 is answered before chardetng is asked. chardetng deliberately
    // never guesses UTF-8 from content alone (its detector is for legacy
    // encodings), so a plain UTF-8 source file would otherwise come back as
    // windows-1252 and render mojibake for every accented character.
    if std::str::from_utf8(prefix).is_ok() {
        return TextEncoding::UTF8;
    }
    // ISO-2022-JP is allowed: this is a file viewer, not a web browser, and
    // the security reason chardetng documents for denying it (a page that can
    // run scripts) does not exist here.
    let mut det = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Allow);
    det.feed(prefix, complete);
    TextEncoding::Rs(det.guess(None, chardetng::Utf8Detection::Allow))
}

/// Does this look like a file to open in hex? ("A file detected
/// as binary opens in hex automatically unless overridden.")
///
/// A NUL byte in the probe is the test, plus a high proportion of other C0
/// control bytes. UTF-16 is deliberately **not** binary: it is full of NULs and
/// it is text, so a UTF-16 BOM excuses it - which is the one case where the
/// encoding decision has to be made before this one.
pub fn looks_binary(prefix: &[u8], encoding: TextEncoding) -> bool {
    let probe = prefix
        .get(..BINARY_PROBE_BYTES.min(prefix.len()))
        .unwrap_or(prefix);
    if probe.is_empty() {
        return false;
    }
    if encoding.line_term().unit() == 2 {
        return false;
    }
    if probe.contains(&0) {
        return true;
    }
    let odd = probe
        .iter()
        .filter(|b| **b < 0x20 && !matches!(**b, b'\n' | b'\r' | b'\t' | 0x0C | 0x1B))
        .count();
    odd.saturating_mul(100) > probe.len().saturating_mul(5)
}

/// The `F8` shortlist.
///
/// > `F8` cycles through a configurable shortlist (UTF-8, the detected one,
/// > `windows-1252`, `cp437`, `utf-16le`) so a mis-detected file is one
/// > keystroke from readable.
///
/// The detected encoding is spliced in second and de-duplicated, so a file
/// sniffed as `windows-1252` gives a four-entry ring rather than a five-entry
/// one with a repeat in it.
pub fn shortlist(detected: TextEncoding) -> Vec<TextEncoding> {
    let mut out = vec![TextEncoding::UTF8, detected];
    for label in ["windows-1252", "cp437", "utf-16le"] {
        if let Some(enc) = encoding_for(label) {
            out.push(enc);
        }
    }
    out.dedup_by(|a, b| a == b);
    let mut seen: Vec<TextEncoding> = Vec::new();
    out.retain(|e| {
        if seen.contains(e) {
            false
        } else {
            seen.push(*e);
            true
        }
    });
    out
}

#[cfg(test)]
mod tests {

    #[test]
    fn utf16_without_a_mark_is_text_and_not_hex() {
        // What a Windows-written `.xml` is: two bytes a character, no byte
        // order mark to say so. Every other byte is a NUL, and the binary test
        // saw them and opened the file in hex.
        let cfg = ViewerEncodingConfig::default();
        let le = b"<\x00?\x00x\x00m\x00l\x00 \x00v\x00e\x00r\x00s\x00i\x00o\x00n\x00?\x00>\x00\n\x00<\x00r\x00o\x00o\x00t\x00/\x00>\x00";
        let found = resolve(&cfg, le, true);
        assert!(
            !looks_binary(le, found.encoding),
            "it reads as the text it is"
        );

        // The other way round, which is the same file written big-endian.
        let be = b"\x00<\x00?\x00x\x00m\x00l\x00 \x00v\x00e\x00r\x00s\x00i\x00o\x00n\x00?\x00>\x00\n\x00<\x00r\x00o\x00o\x00t";
        let found = resolve(&cfg, be, true);
        assert!(!looks_binary(be, found.encoding), "either way round");
    }

    #[test]
    fn a_real_binary_is_still_a_binary() {
        // The guard on the rule above: NULs in no pattern, and arbitrary bytes
        // between them. Nothing here should be mistaken for a document.
        let cfg = ViewerEncodingConfig::default();
        let elf = b"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x03\x00>\x00\x01\x00\x00\x00\x50\x1a\x00\x00\x00\x00\x00\x00\x40\x00\x00\x00\x00\x00\x00\x00";
        let found = resolve(&cfg, elf, true);
        assert!(looks_binary(elf, found.encoding), "an ELF header is binary");

        let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x01\x00\x00\x00\x01\x00\x08\x06\x00\x00\x00\x5c\x72\xa8\x66";
        let found = resolve(&cfg, png, true);
        assert!(looks_binary(png, found.encoding), "so is a PNG");

        // Half NULs, but not alternating and not text between them.
        let noise =
            b"\x00\x01\x00\x02\xff\x00\x00\x03\x00\x00\xfe\x00\x04\x00\x00\x05\x00\xfd\x00\x06";
        let found = resolve(&cfg, noise, true);
        assert!(
            looks_binary(noise, found.encoding),
            "NULs alone are not a pattern"
        );
    }
    use super::*;

    #[test]
    fn labels_resolve_through_both_tables() {
        assert_eq!(
            encoding_for("utf-8").map(TextEncoding::label),
            Some("UTF-8")
        );
        assert_eq!(
            encoding_for("latin1").map(TextEncoding::label),
            Some("windows-1252"),
            "the WHATWG alias table is encoding_rs's"
        );
        assert_eq!(
            encoding_for("CP437").map(TextEncoding::label),
            Some("cp437"),
            "and cp437 is ours, because the WHATWG set does not carry it"
        );
        assert_eq!(encoding_for("klingon"), None);
        assert_eq!(encoding_for("   "), None);
    }

    #[test]
    fn cp437_decodes_the_box_drawing_range() {
        let enc = encoding_for("cp437").expect("cp437");
        let (text, errors) = enc.decode(&[0xC9, 0xCD, 0xBB, b'A', 0x0A]);
        assert_eq!(text, "╔═╗A\n");
        assert!(!errors, "a single-byte page has no invalid sequences");
    }

    #[test]
    fn an_invalid_sequence_is_a_glyph_and_not_a_failure() {
        let (text, errors) = TextEncoding::UTF8.decode(b"ok \xff\xfe bad");
        assert!(text.contains('\u{fffd}'), "{text:?}");
        assert!(errors, "and it is reported rather than hidden");
    }

    #[test]
    fn a_bom_is_recognised_and_measured() {
        assert_eq!(
            bom(b"\xef\xbb\xbfhi").map(|(e, n)| (e.label(), n)),
            Some(("UTF-8", 3))
        );
        assert_eq!(
            bom(b"\xff\xfeh\0").map(|(e, n)| (e.label(), n)),
            Some(("UTF-16LE", 2))
        );
        assert_eq!(bom(b"plain"), None);
    }

    #[test]
    fn a_bom_wins_over_the_configured_default() {
        let cfg = ViewerEncodingConfig {
            default: "windows-1252".to_string(),
            detect: true,
            fallback: "windows-1252".to_string(),
            bom: true,
            shortlist: Vec::new(),
        };
        let got = resolve(&cfg, b"\xef\xbb\xbfhello", true);
        assert_eq!(got.encoding.label(), "UTF-8");
        assert_eq!(got.how, Detected::Bom);
        assert_eq!(got.bom_len, 3);

        // ...and does not when the setting is off.
        let cfg = ViewerEncodingConfig { bom: false, ..cfg };
        let got = resolve(&cfg, b"\xef\xbb\xbfhello", true);
        assert_eq!(got.encoding.label(), "windows-1252");
        assert_eq!(got.how, Detected::Configured);
        assert_eq!(got.bom_len, 0);
    }

    #[test]
    fn auto_sniffs_and_utf8_is_answered_without_asking() {
        let cfg = ViewerEncodingConfig::default();
        assert_eq!(cfg.default, "auto");
        let got = resolve(&cfg, "héllo wörld".as_bytes(), true);
        assert_eq!(got.encoding.label(), "UTF-8");
        assert_eq!(got.how, Detected::Sniffed);

        // Latin-1 bytes are not valid UTF-8, so chardetng gets a say.
        let got = resolve(&cfg, b"caf\xe9 na\xefve cr\xe8me br\xfbl\xe9e", true);
        assert!(got.encoding.label().starts_with("windows-"), "{got:?}");
    }

    #[test]
    fn an_unknown_label_falls_through_rather_than_refusing_to_open() {
        let cfg = ViewerEncodingConfig {
            default: "klingon".to_string(),
            detect: false,
            fallback: "also-klingon".to_string(),
            bom: true,
            shortlist: Vec::new(),
        };
        let got = resolve(&cfg, b"hello", true);
        assert_eq!(got.encoding, TextEncoding::UTF8);
        assert_eq!(got.how, Detected::Fallback);
    }

    #[test]
    fn utf8_resync_walks_back_to_the_lead_byte() {
        // "é" is C3 A9; a window starting on the A9 must not decode it alone.
        let bytes = "aéb".as_bytes();
        assert_eq!(bytes, b"a\xc3\xa9b");
        let (at, approx) = resync(TextEncoding::UTF8, 0, bytes, 2);
        assert_eq!(at, 1, "back to the C3");
        assert!(!approx, "and it is exact, not a guess");
        assert_eq!(resync(TextEncoding::UTF8, 0, bytes, 1), (1, false));
        assert_eq!(resync(TextEncoding::UTF8, 0, bytes, 3), (3, false));
    }

    #[test]
    fn utf16_resync_lands_on_the_code_unit_grid() {
        let enc = encoding_for("utf-16le").expect("utf-16le");
        assert_eq!(resync(enc, 0, b"", 7), (6, false));
        assert_eq!(resync(enc, 0, b"", 8), (8, false));
    }

    #[test]
    fn a_stateful_encoding_admits_that_its_start_is_a_guess() {
        let enc = encoding_for("shift_jis").expect("shift_jis");
        assert_eq!(enc.resync(), Resync::LeadIn);
        let (at, approx) = resync(enc, 0, b"", 100_000);
        assert_eq!(at, 100_000 - Resync::LEAD_IN);
        assert!(approx, "the honesty rule, applied to decoding");
    }

    #[test]
    fn a_single_byte_encoding_resyncs_anywhere() {
        let enc = encoding_for("windows-1252").expect("cp1252");
        assert_eq!(enc.resync(), Resync::Any);
        assert_eq!(resync(enc, 0, b"", 12_345), (12_345, false));
    }

    #[test]
    fn utf16_line_breaks_are_found_on_the_unit_grid_and_not_the_byte_grid() {
        // U+0A41 is `41 0A` little-endian: a 0x0A byte that is not a break.
        let bytes = b"\x41\x0a\x0a\x00X\x00";
        assert_eq!(LineTerm::Utf16Le.find(bytes, 0), Some(2));
        assert_eq!(
            LineTerm::Byte.find(bytes, 0),
            Some(1),
            "a naive byte scan gets it wrong, which is why LineTerm exists"
        );
        assert_eq!(LineTerm::Utf16Le.rfind(bytes, 6), Some(2));
        assert_eq!(LineTerm::Utf16Le.rfind(bytes, 2), None);
    }

    #[test]
    fn trim_break_takes_the_carriage_return_with_it() {
        assert_eq!(LineTerm::Byte.trim_break(b"line\r\n"), b"line");
        assert_eq!(LineTerm::Byte.trim_break(b"line\n"), b"line");
        assert_eq!(LineTerm::Byte.trim_break(b"line"), b"line");
        assert_eq!(LineTerm::Utf16Le.trim_break(b"l\0\r\0\n\0"), b"l\0");
        assert_eq!(LineTerm::Utf16Be.trim_break(b"\0l\0\r\0\n"), b"\0l");
    }

    #[test]
    fn binary_detection_lets_utf16_through() {
        assert!(looks_binary(b"\x7fELF\0\0\0", TextEncoding::UTF8));
        assert!(!looks_binary(b"plain text\n", TextEncoding::UTF8));
        let utf16 = encoding_for("utf-16le").expect("utf-16le");
        assert!(
            !looks_binary(b"h\0i\0\n\0", utf16),
            "UTF-16 is full of NULs and is not binary"
        );
        assert!(!looks_binary(b"", TextEncoding::UTF8));
    }

    #[test]
    fn the_shortlist_never_repeats_itself() {
        let list = shortlist(encoding_for("windows-1252").expect("cp1252"));
        let labels: Vec<&str> = list.iter().map(|e| e.label()).collect();
        assert_eq!(labels, ["UTF-8", "windows-1252", "cp437", "UTF-16LE"]);

        let list = shortlist(encoding_for("koi8-r").expect("koi8-r"));
        let labels: Vec<&str> = list.iter().map(|e| e.label()).collect();
        assert_eq!(
            labels,
            ["UTF-8", "KOI8-R", "windows-1252", "cp437", "UTF-16LE"]
        );
    }
}
