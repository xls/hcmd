//! Choosing an encoding, and decoding a window whose edges cut a character in
//! half.
//!
//! > `encoding_rs` decodes; **every encoding it supports is selectable**. …
//! > `F8` cycles through a **configurable shortlist** (UTF-8, the detected one,
//! > `windows-1252`, `cp437`, `utf-16le`) so a mis-detected file is one
//! > keystroke from readable. Decoding is incremental and window-local; changing
//! > encoding re-decodes the visible window only, not the file. Invalid
//! > sequences render as a replacement glyph rather than failing the open.
//!
//! [`super::decode`] is the model - what an encoding *is*, how a decoder can be
//! restarted at an arbitrary offset ([`super::decode::resync`]), and where line
//! breaks live. This module is the two things the design asks for on top of it:
//!
//! 1. **The other end of the window.** `decode::resync` answers "where may I
//!    *start* decoding"; nothing answered "where must I *stop*". A window that
//!    ends three bytes into a four-byte GB18030 character has an incomplete
//!    sequence at its tail, and a one-shot decode turns that into U+FFFD - a
//!    wrong character invented out of bytes that are perfectly good and simply
//!    continue in the next window. [`incomplete_tail`] measures that tail so the
//!    caller can hand those bytes to the next window instead of eating them, and
//!    [`decode_window`] does both halves in one call.
//!
//!    This preserves the streaming invariant rather than weakening it: nothing
//!    is carried between windows *except a byte count*, so decoding is still
//!    stateless per window (the design I8) and changing the encoding
//!    still costs one window and not one file.
//!
//! 2. **The lists.** [`all`] is every encoding that is selectable, which is what
//!    makes "every encoding it supports is selectable" checkable rather than
//!    aspirational, and [`shortlist`] is the `F8` ring built from a configured
//!    list of labels - the "configurable" half that
//!    `decode::shortlist` hard-codes.
//!
//! # How the tail is measured
//!
//! Three of the four encoding classes have an exact local rule and get one:
//! UTF-8 walks back at most three bytes to a lead byte and compares the
//! sequence's declared length against what is actually there; a two-byte-unit
//! encoding rounds to the unit grid and additionally holds back a trailing high
//! surrogate, which is half of a character whose other half is in the next
//! window; a single-byte encoding never has a tail at all.
//!
//! The fourth class - the variable-width and stateful encodings, `Resync::LeadIn`
//! in [`super::decode`] - has no local rule, so it is measured by **asking the
//! decoder**: feed the bytes, flush, and see whether the flush produces
//! anything. If it does, something was held back, and the shortest suffix whose
//! removal makes the flush silent is the tail. That costs at most
//! [`MAX_TAIL`] + 1 passes over the window and is only ever reached when a tail
//! genuinely exists, which is once per window rather than once per line.

use encoding_rs::{DecoderResult, Encoding};

use super::decode::{self, TextEncoding};

/// The largest incomplete tail any encoding carried here can leave.
///
/// GB18030's longest sequence is four bytes and ISO-2022-JP's longest escape
/// sequence is five; eight is comfortably above both and bounds the probe.
pub const MAX_TAIL: usize = 8;

/// The scratch buffer the tail probe decodes into. Fixed, so the probe
/// allocates nothing whatever the window size.
const PROBE_BUF: usize = 1024;

/// A ceiling on the probe's inner loop, so a decoder that somehow consumed
/// nothing cannot spin.
const PROBE_STEPS: usize = 64 * 1024;

/// The canonical name of every encoding `encoding_rs` implements
/// ("every encoding it supports is selectable").
///
/// `replacement` is deliberately absent. It is the WHATWG standard's security
/// construct - it maps a whole document to a single U+FFFD so that a page
/// cannot smuggle ASCII through a mis-labelled charset - and offering it in a
/// file viewer's encoding list would only ever be a way to make a file
/// unreadable.
static WHATWG_NAMES: &[&str] = &[
    "UTF-8",
    "UTF-16LE",
    "UTF-16BE",
    "windows-1250",
    "windows-1251",
    "windows-1252",
    "windows-1253",
    "windows-1254",
    "windows-1255",
    "windows-1256",
    "windows-1257",
    "windows-1258",
    "windows-874",
    "ISO-8859-2",
    "ISO-8859-3",
    "ISO-8859-4",
    "ISO-8859-5",
    "ISO-8859-6",
    "ISO-8859-7",
    "ISO-8859-8",
    "ISO-8859-8-I",
    "ISO-8859-10",
    "ISO-8859-13",
    "ISO-8859-14",
    "ISO-8859-15",
    "ISO-8859-16",
    "KOI8-R",
    "KOI8-U",
    "IBM866",
    "macintosh",
    "x-mac-cyrillic",
    "Big5",
    "EUC-JP",
    "EUC-KR",
    "GBK",
    "gb18030",
    "Shift_JIS",
    "ISO-2022-JP",
    "x-user-defined",
];

/// The default `F8` ring when nothing is configured.
///
/// `auto` is the placeholder for whatever the file was detected as, so the
/// ring a user writes in `config.toml` can say where the detected encoding
/// goes rather than always having it spliced in second.
pub const DEFAULT_SHORTLIST: &[&str] = &["utf-8", "auto", "windows-1252", "cp437", "utf-16le"];

/// The label a configured shortlist uses for "whatever this file was detected
/// as".
pub const DETECTED_LABEL: &str = "auto";

/// Every encoding the viewer can be switched to, in a stable order.
///
///
/// The `encoding_rs` set first, then the code pages [`super::decode`] carries
/// itself. Anything `encoding_rs` fails to resolve is skipped rather than
/// faked, so this list only ever contains encodings that really decode.
pub fn all() -> Vec<TextEncoding> {
    let mut out: Vec<TextEncoding> = WHATWG_NAMES
        .iter()
        .filter_map(|name| Encoding::for_label(name.as_bytes()))
        .map(TextEncoding::Rs)
        .collect();
    for page in decode::CODE_PAGES {
        out.push(TextEncoding::Page(page));
    }
    out
}

/// The selectable encodings whose label contains `query`, case-insensitively.
///
/// An empty query is every encoding, which is what an encoding picker opens on.
pub fn matching(query: &str) -> Vec<TextEncoding> {
    let needle = query.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return all();
    }
    // An exact label - including every WHATWG alias, which `encoding_for`
    // resolves and a substring search would miss - comes first.
    let exact = decode::encoding_for(&needle);
    let mut out: Vec<TextEncoding> = exact.into_iter().collect();
    for enc in all() {
        if enc.label().to_ascii_lowercase().contains(&needle) && !out.contains(&enc) {
            out.push(enc);
        }
    }
    out
}

/// The `F8` ring, built from a configured list of labels.
///
/// `labels` are `viewer.encoding.shortlist` as written by the user; the entry
/// [`DETECTED_LABEL`] stands for `detected`. An empty or wholly unrecognised
/// list falls back to [`super::decode::shortlist`], which is the own list - a
/// typo in the config makes `F8` ordinary again rather than breaking it.
///
/// **The detected encoding is always in the ring**, spliced in second if the
/// configured list left it out. Without that, `F8` on a file that opened as
/// `koi8-r` could never cycle back to the state the file opened in, and a ring
/// you cannot get back to the start of is worse than no ring.
pub fn shortlist(labels: &[String], detected: TextEncoding) -> Vec<TextEncoding> {
    let mut out: Vec<TextEncoding> = Vec::new();
    for label in labels {
        let trimmed = label.trim();
        let found = if trimmed.eq_ignore_ascii_case(DETECTED_LABEL) {
            Some(detected)
        } else {
            decode::encoding_for(trimmed)
        };
        if let Some(enc) = found
            && !out.contains(&enc)
        {
            out.push(enc);
        }
    }
    if out.is_empty() {
        return decode::shortlist(detected);
    }
    if !out.contains(&detected) {
        out.insert(1.min(out.len()), detected);
    }
    out
}

/// One window's worth of decoded text, and where the next window must start.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WindowText {
    /// The decoded text. Invalid sequences are already U+FFFD (
    /// "invalid sequences render as a replacement glyph rather than failing the
    /// open").
    pub text: String,
    /// True when something in `text` came from an invalid sequence.
    pub had_errors: bool,
    /// How many of the input bytes were decoded. The next window starts
    /// `used` bytes on, **not** at the end of the input.
    pub used: usize,
    /// How many trailing bytes were an incomplete sequence and were deliberately
    /// not decoded. `used + tail == bytes.len()`.
    pub tail: usize,
}

/// Decode one window.
///
/// `last` says the window reached the end of the file. At the end of a file
/// there is no next window to carry a partial sequence into, so an incomplete
/// tail there really is malformed and becomes a replacement glyph - which is
/// the rule, not a failure.
///
/// Nothing is carried between calls but [`WindowText::used`]: no decoder, no
/// state. That is what keeps "changing encoding re-decodes the visible window
/// only" true.
pub fn decode_window(enc: TextEncoding, bytes: &[u8], last: bool) -> WindowText {
    let tail = if last { 0 } else { incomplete_tail(enc, bytes) };
    let used = bytes.len().saturating_sub(tail);
    let body = bytes.get(..used).unwrap_or(&[]);
    let (text, had_errors) = decode_body(enc, body);
    WindowText {
        text,
        had_errors,
        used,
        tail,
    }
}

/// Decode a complete run of bytes, with **no BOM handling**.
///
/// `encoding_rs`'s `Encoding::decode` sniffs a BOM off the front and will
/// switch encoding because of it. That is right for a whole document and wrong
/// for a window: a window that happens to begin with the bytes `EF BB BF`
/// halfway down a file would silently lose three bytes of content, and one
/// beginning `FF FE` would switch to UTF-16 for that window alone. The viewer
/// resolves the file's BOM once, at open, in [`super::decode::resolve`], and
/// skips it by starting at `bom_len`; every window after that is content.
fn decode_body(enc: TextEncoding, bytes: &[u8]) -> (String, bool) {
    match enc {
        TextEncoding::Rs(rs) => {
            let (text, had_errors) = rs.decode_without_bom_handling(bytes);
            (text.into_owned(), had_errors)
        }
        // A code page is a byte-per-character table with no BOM notion at all,
        // so `decode` is already window-safe.
        TextEncoding::Page(_) => enc.decode(bytes),
    }
}

/// How many trailing bytes of `bytes` are an **incomplete** sequence.
///
///
/// Never more than [`MAX_TAIL`], never more than `bytes.len()`. Zero for a
/// single-byte encoding, and zero whenever the last character ends exactly on
/// the boundary - which is the common case, so the common case costs a glance
/// at the last few bytes.
pub fn incomplete_tail(enc: TextEncoding, bytes: &[u8]) -> usize {
    let tail = match enc {
        // A code page decodes one byte to one character. There is no sequence
        // to be halfway through.
        TextEncoding::Page(_) => 0,
        TextEncoding::Rs(rs) => match enc.resync() {
            decode::Resync::Any => 0,
            decode::Resync::Utf8 => utf8_tail(bytes),
            decode::Resync::Unit(_) => utf16_tail(rs, bytes),
            decode::Resync::LeadIn => probe_tail(rs, bytes),
        },
    };
    tail.min(bytes.len()).min(MAX_TAIL)
}

/// UTF-8's exact local rule.
///
/// A UTF-8 sequence declares its own length in its lead byte and is at most
/// four bytes long, so the answer is at most three bytes back from the end.
fn utf8_tail(bytes: &[u8]) -> usize {
    let len = bytes.len();
    for back in 1..=4_usize {
        if back > len {
            return 0;
        }
        let idx = len.saturating_sub(back);
        let Some(byte) = bytes.get(idx).copied() else {
            return 0;
        };
        if byte & 0xC0 == 0x80 {
            // A continuation byte: the sequence began earlier still.
            continue;
        }
        let need = match byte {
            0x00..=0x7F => 1,
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            // Not a legal lead byte at all: malformed, and malformed is the
            // decoder's business rather than a boundary to hold back.
            _ => return 0,
        };
        return if back < need { back } else { 0 };
    }
    // Four continuation bytes in a row is malformed however it is cut.
    0
}

/// UTF-16's exact local rule: the code-unit grid, plus a lone high surrogate.
///
/// A surrogate pair is one character in two code units, and a window that ends
/// between them has half a character in hand. Holding both units back is what
/// stops the pair decoding as two U+FFFDs across the boundary.
fn utf16_tail(rs: &'static Encoding, bytes: &[u8]) -> usize {
    let odd = bytes.len() % 2;
    let whole = bytes.len().saturating_sub(odd);
    if whole < 2 {
        return bytes.len();
    }
    let (lo, hi) = (whole.saturating_sub(2), whole.saturating_sub(1));
    let (b0, b1) = (
        bytes.get(lo).copied().unwrap_or(0),
        bytes.get(hi).copied().unwrap_or(0),
    );
    let unit = if rs == encoding_rs::UTF_16BE {
        u16::from(b0) << 8 | u16::from(b1)
    } else {
        u16::from(b1) << 8 | u16::from(b0)
    };
    if (0xD800..=0xDBFF).contains(&unit) {
        odd.saturating_add(2)
    } else {
        odd
    }
}

/// The general rule, for the encodings that have no local one.
///
/// Only ever runs when a tail actually exists, because the first `holds_back`
/// call is the test for that. When it does, the shortest suffix whose removal
/// makes the decoder flush silently is the tail, and that is exact.
fn probe_tail(rs: &'static Encoding, bytes: &[u8]) -> usize {
    if !holds_back(rs, bytes) {
        return 0;
    }
    let len = bytes.len();
    for k in 1..=MAX_TAIL {
        if k > len {
            break;
        }
        let shorter = bytes.get(..len.saturating_sub(k)).unwrap_or(&[]);
        if !holds_back(rs, shorter) {
            return k;
        }
    }
    // Nothing within MAX_TAIL made the flush silent. Decoding the lot and
    // letting the malformed bytes become replacement glyphs is the honest
    // answer: a bad sequence is a glyph, not a refusal.
    0
}

/// Does decoding `bytes` leave the decoder holding something back?
///
/// Feeds `bytes` with `last = false` - which is what makes the decoder keep an
/// unfinished sequence rather than replacing it - and then flushes with
/// `last = true`. A flush that produces output, or reports a malformed
/// sequence, is the decoder saying it had bytes in hand.
///
/// The output is thrown away into a fixed buffer, so this allocates nothing.
fn holds_back(rs: &'static Encoding, bytes: &[u8]) -> bool {
    let mut decoder = rs.new_decoder_without_bom_handling();
    let mut sink = [0_u8; PROBE_BUF];
    let mut src = bytes;
    for _ in 0..PROBE_STEPS {
        let (result, read, _written) =
            decoder.decode_to_utf8_without_replacement(src, &mut sink, false);
        src = src.get(read..).unwrap_or(&[]);
        match result {
            DecoderResult::InputEmpty => break,
            // A malformed sequence in the middle is not what is being measured;
            // step past it and keep going, exactly as a replacing decoder would.
            DecoderResult::OutputFull | DecoderResult::Malformed(_, _) => {
                if read == 0 && src.is_empty() {
                    break;
                }
            }
        }
    }
    let (result, _read, written) = decoder.decode_to_utf8_without_replacement(&[], &mut sink, true);
    written > 0 || matches!(result, DecoderResult::Malformed(_, _))
}

// ------------------------------------------- one character at a time --------
//
// the design gives the characters side of the hex view, and the arrows in text
// mode, a unit of **one character** - "which under a multi-byte encoding may be
// several bytes". These three answer that, and they are the only place in the
// crate that does, so movement and layout cannot disagree about where a
// character begins.
//
// Window-local and stateless: a decoder is created, used and dropped inside
// each call exactly as [`decode_window`] does, so the design I8 -
// no decoder is cached across calls - still holds.

/// The longest a single character can be in any encoding this viewer decodes.
///
///
/// Eight is generous: UTF-8 needs four and every legacy encoding in the
/// shortlist needs at most two, but an ISO-2022 escape sequence is longer than
/// either and a bound that is too small would silently truncate a step.
pub const MAX_CHAR_BYTES: usize = 8;

/// Byte offsets into `bytes` at which each decoded character begins.
///
///
/// The whole window, in one pass, for a caller that needs every boundary rather
/// than the next one - [`super::cursor::cells_of`] is the one that does.
pub fn char_starts(enc: TextEncoding, bytes: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut at = 0_usize;
    while at < bytes.len() {
        out.push(at);
        // `next_char` is at least 1, so this cannot fail to advance.
        at = at.saturating_add(next_char(enc, bytes, at));
    }
    out
}

/// How many bytes the character starting at `at` occupies. **At least 1**, so a
/// caller can never fail to advance.
///
/// Decided by decoding rather than by a table of lead-byte shapes, because
/// the encoding list is `encoding_rs`'s and this program does not
/// hold a second, private opinion about how any of them is spelled. The
/// shortest prefix that decodes to something, consuming exactly itself, is the
/// character; a byte that begins nothing decodable is one byte, which is what
/// makes an arrow key still move over a malformed run.
pub fn next_char(enc: TextEncoding, bytes: &[u8], at: usize) -> usize {
    let rest = bytes.len().saturating_sub(at);
    if rest == 0 {
        return 1;
    }
    let probe = bytes.get(at..at.saturating_add(MAX_CHAR_BYTES).min(bytes.len()));
    let probe = probe.unwrap_or(&[]);
    for take in 1..=probe.len() {
        let head = probe.get(..take).unwrap_or(&[]);
        // `last` is false: a prefix that is merely *incomplete* must not be
        // reported as a whole character, or `Right` would stop inside one.
        let decoded = decode_window(enc, head, false);
        if decoded.used == take && !decoded.text.is_empty() {
            return take;
        }
    }
    1
}

/// Where the character ending at `before` starts. **At most `before - 1`**, so
/// a caller can never fail to retreat.
///
/// Walked forwards from at most [`MAX_CHAR_BYTES`] back, because no encoding
/// here can be read backwards: a trail byte does not say how many led up to it
/// in every encoding, and guessing is how a cursor ends up inside a character.
pub fn prev_char(enc: TextEncoding, bytes: &[u8], before: usize) -> usize {
    let before = before.min(bytes.len());
    if before == 0 {
        return 0;
    }
    let floor = before.saturating_sub(MAX_CHAR_BYTES);
    let mut best = before.saturating_sub(1);
    let mut at = floor;
    while at < before {
        let n = next_char(enc, bytes, at);
        if at.saturating_add(n) >= before {
            best = at;
            break;
        }
        at = at.saturating_add(n);
    }
    best.min(before.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(label: &str) -> TextEncoding {
        decode::encoding_for(label).unwrap_or(TextEncoding::UTF8)
    }

    /// `text` written in `label`'s bytes.
    ///
    /// `encoding_rs` deliberately refuses to *encode* UTF-16 - `encode` maps
    /// UTF-16LE and UTF-16BE onto UTF-8 output, which is the WHATWG rule for a
    /// form submission and would silently produce the wrong bytes here - so
    /// those two are laid out by hand.
    fn bytes_of(label: &str, text: &str) -> Vec<u8> {
        match enc(label) {
            TextEncoding::Rs(rs) if rs == encoding_rs::UTF_16LE => {
                text.encode_utf16().flat_map(u16::to_le_bytes).collect()
            }
            TextEncoding::Rs(rs) if rs == encoding_rs::UTF_16BE => {
                text.encode_utf16().flat_map(u16::to_be_bytes).collect()
            }
            TextEncoding::Rs(rs) => {
                let (out, _, unmappable) = rs.encode(text);
                assert!(!unmappable, "{label} cannot write {text:?}");
                out.into_owned()
            }
            TextEncoding::Page(page) => text
                .chars()
                .map(|c| {
                    let at = page.table.iter().position(|t| *t == c);
                    u8::try_from(at.unwrap_or(0)).unwrap_or(0)
                })
                .collect(),
        }
    }

    /// The property the whole module exists for: cutting a byte run anywhere
    /// and decoding the two halves as windows must produce exactly what
    /// decoding the whole run produces - no invented character, no lost byte.
    ///
    /// Stated for the **stateless** encodings. A stateful one (ISO-2022-JP)
    /// cannot satisfy it with a fresh decoder per window and never claimed to:
    /// `decode::Resync::LeadIn` is where the viewer says so. Its own weaker
    /// guarantee - every byte accounted for, and an escape sequence never cut -
    /// is asserted separately below.
    fn every_cut_is_lossless(label: &str, text: &str) {
        let e = enc(label);
        let bytes = bytes_of(label, text);
        let whole = decode_window(e, &bytes, true);
        assert!(
            !whole.text.contains('\u{fffd}'),
            "{label}: {:?}",
            whole.text
        );

        for cut in 0..=bytes.len() {
            let head = bytes.get(..cut).unwrap_or(&[]);
            let first = decode_window(e, head, false);
            // Everything the first window did not decode is handed on.
            let rest = bytes.get(first.used..).unwrap_or(&[]);
            let second = decode_window(e, rest, true);
            let joined = format!("{}{}", first.text, second.text);
            assert_eq!(
                joined, whole.text,
                "{label}: cut at {cut} lost or invented a character \
                 (tail {}, used {})",
                first.tail, first.used
            );
            assert_eq!(
                first.used.saturating_add(first.tail),
                head.len(),
                "{label}: cut at {cut} accounts for every byte"
            );
        }
    }

    #[test]
    fn a_utf8_window_boundary_never_splits_a_character() {
        every_cut_is_lossless("utf-8", "aé€𝄞b naïve 日 ok");
    }

    #[test]
    fn a_utf16_window_boundary_holds_back_half_a_surrogate_pair() {
        // U+1D11E is a surrogate pair, which is the case a unit-grid rule alone
        // gets wrong.
        every_cut_is_lossless("utf-16le", "a𝄞bé日");
        every_cut_is_lossless("utf-16be", "a𝄞bé日");
    }

    #[test]
    fn a_variable_width_window_boundary_is_measured_by_asking_the_decoder() {
        every_cut_is_lossless("shift_jis", "あいうえおabc");
        every_cut_is_lossless("euc-jp", "かきくけこxyz");
        every_cut_is_lossless("big5", "中文測試abc");
        // GB18030's four-byte sequences are the longest tail there is.
        every_cut_is_lossless("gb18030", "中文\u{20000}abc");
    }

    #[test]
    fn a_stateful_encoding_never_has_its_escape_sequence_cut_in_half() {
        // ISO-2022-JP switches charset with an escape sequence, so a window can
        // end halfway through one. Holding the partial escape back is what
        // stops it decoding as two stray ASCII characters and the next window
        // then decoding Japanese as Latin.
        //
        // It is *not* the full round-trip property: the charset state itself
        // does not survive a fresh decoder, which is exactly what
        // `decode::Resync::LeadIn` admits about this class of encoding.
        let e = enc("iso-2022-jp");
        let bytes = bytes_of("iso-2022-jp", "abcあいうdef");
        let escape = bytes
            .windows(3)
            .position(|w| w == b"\x1b$B")
            .expect("an escape sequence");
        for cut in 0..=bytes.len() {
            let head = bytes.get(..cut).unwrap_or(&[]);
            let got = decode_window(e, head, false);
            assert_eq!(
                got.used.saturating_add(got.tail),
                head.len(),
                "cut at {cut} lost a byte"
            );
            assert!(got.tail <= MAX_TAIL);
            if cut > escape && cut < escape.saturating_add(3) {
                assert!(
                    got.tail > 0,
                    "cut at {cut} is inside the escape sequence at {escape} \
                     and must be held back"
                );
            }
        }
    }

    #[test]
    fn a_single_byte_encoding_has_no_tail_at_all() {
        let e = enc("windows-1252");
        for cut in 0..8_usize {
            let bytes = vec![0xE9_u8; cut];
            assert_eq!(incomplete_tail(e, &bytes), 0);
        }
        let cp437 = enc("cp437");
        assert_eq!(incomplete_tail(cp437, &[0xC9, 0xCD, 0xBB]), 0);
    }

    #[test]
    fn the_end_of_the_file_decodes_its_tail_rather_than_holding_it_forever() {
        // "é" is C3 A9; alone, the C3 is genuinely malformed at end of file.
        let cut = decode_window(TextEncoding::UTF8, b"a\xc3", false);
        assert_eq!(cut.tail, 1, "held back mid-file");
        assert_eq!(cut.text, "a");

        let last = decode_window(TextEncoding::UTF8, b"a\xc3", true);
        assert_eq!(last.tail, 0);
        assert!(last.text.contains('\u{fffd}'), "{:?}", last.text);
        assert!(last.had_errors, "and it is reported, not hidden");
    }

    #[test]
    fn a_window_that_begins_with_bom_bytes_keeps_them() {
        // encoding_rs's `decode` would eat these three bytes and could switch
        // encoding because of them. A window in the middle of a file is
        // content, not a document start.
        let got = decode_window(TextEncoding::UTF8, b"\xef\xbb\xbfx", true);
        assert_eq!(got.text, "\u{feff}x", "the BOM is a character here");
        let utf16 = enc("utf-16le");
        let got = decode_window(utf16, b"\xff\xfeA\x00", true);
        assert_eq!(got.text, "\u{feff}A");
    }

    #[test]
    fn a_malformed_run_is_glyphs_and_not_an_endless_tail() {
        // Four continuation bytes with no lead byte: nothing to hold back.
        assert_eq!(incomplete_tail(TextEncoding::UTF8, b"\x80\x80\x80\x80"), 0);
        // An over-long lead byte that no longer exists in UTF-8.
        assert_eq!(incomplete_tail(TextEncoding::UTF8, b"a\xfe"), 0);
        let got = decode_window(TextEncoding::UTF8, b"a\xfe", false);
        assert!(got.had_errors);
        assert!(got.text.contains('\u{fffd}'));
    }

    #[test]
    fn the_tail_is_bounded_however_odd_the_input() {
        for label in ["utf-8", "utf-16le", "shift_jis", "gb18030", "iso-2022-jp"] {
            let e = enc(label);
            for len in 0..24_usize {
                let bytes: Vec<u8> = (0..len).map(|i| 0x80_u8.wrapping_add(i as u8)).collect();
                let tail = incomplete_tail(e, &bytes);
                assert!(
                    tail <= MAX_TAIL && tail <= bytes.len(),
                    "{label} {len} {tail}"
                );
            }
        }
    }

    #[test]
    fn every_encoding_encoding_rs_supports_is_selectable() {
        let list = all();
        assert!(list.len() >= 39, "{} encodings", list.len());
        for name in WHATWG_NAMES {
            assert!(
                list.iter().any(|e| e.label().eq_ignore_ascii_case(name)),
                "{name} is missing from the selectable list"
            );
        }
        assert!(
            list.iter().any(|e| e.label() == "cp437"),
            "the carried code pages are selectable too"
        );
        assert!(
            !list.iter().any(|e| e.label() == "replacement"),
            "the WHATWG replacement encoding is not an encoding a user wants"
        );
    }

    #[test]
    fn matching_finds_by_alias_and_by_substring() {
        assert_eq!(
            matching("latin1").first().map(|e| e.label()),
            Some("windows-1252"),
            "an alias resolves exactly and comes first"
        );
        let iso = matching("iso-8859");
        assert!(iso.len() >= 13, "{} matches", iso.len());
        assert!(matching("cp4").iter().any(|e| e.label() == "cp437"));
        assert_eq!(matching("  ").len(), all().len());
        assert!(matching("klingon").is_empty());
    }

    #[test]
    fn a_configured_shortlist_is_honoured_in_order() {
        let labels: Vec<String> = ["utf-8", "koi8-r", "auto"]
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        let list = shortlist(&labels, enc("windows-1251"));
        let got: Vec<&str> = list.iter().map(|e| e.label()).collect();
        assert_eq!(got, ["UTF-8", "KOI8-R", "windows-1251"]);
    }

    #[test]
    fn the_detected_encoding_is_always_in_the_ring() {
        let labels: Vec<String> = ["utf-8", "cp437"]
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        let list = shortlist(&labels, enc("big5"));
        let got: Vec<&str> = list.iter().map(|e| e.label()).collect();
        assert_eq!(
            got,
            ["UTF-8", "Big5", "cp437"],
            "otherwise F8 could never cycle back to how the file opened"
        );
    }

    #[test]
    fn an_empty_or_nonsense_shortlist_falls_back_to_the_spec_list() {
        let detected = enc("koi8-r");
        assert_eq!(shortlist(&[], detected), decode::shortlist(detected));
        let junk: Vec<String> = ["klingon", "  "]
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        assert_eq!(shortlist(&junk, detected), decode::shortlist(detected));
    }

    #[test]
    fn the_default_shortlist_is_the_one_spec_10_4_writes_down() {
        let labels: Vec<String> = DEFAULT_SHORTLIST
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        let list = shortlist(&labels, enc("koi8-r"));
        let got: Vec<&str> = list.iter().map(|e| e.label()).collect();
        assert_eq!(
            got,
            ["UTF-8", "KOI8-R", "windows-1252", "cp437", "UTF-16LE"],
            "and it agrees with decode::shortlist, which hard-codes it"
        );
        assert_eq!(list, decode::shortlist(enc("koi8-r")));
    }
}
