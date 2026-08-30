//! Quick find inside the viewer.
//!
//! > `F7`, `/` or `Ctrl+F` opens the find bar. It behaves like the panel quick
//! > search: **typing searches immediately**, incrementally, with the first
//! > match highlighted as you type. … Searching is **streamed over the file,
//! > not over a loaded buffer**, so it starts returning hits on a huge file
//! > immediately. A match counter fills in behind the cursor while the scan
//! > continues in the background, cancellable with `Esc`.
//!
//! # The two searches, and why there are two
//!
//! `n` and the first match as you type are **exact and immediate**: they stream
//! forward (or backward) from the cursor through [`super::source::Source`],
//! stop at the first hit, and cost a bounded number of windows per call
//! ([`FIND_READ_BUDGET`]). They never consult a list of hits, so they are right
//! on the first keystroke over a 40 GB file, before any background work has
//! happened at all.
//!
//! The **counter** - `3/57` - is the background half. [`scan`] walks the file
//! once on the blocking pool, exactly as [`super::index::scan`] does, and posts
//! [`FindBatch`]es. `Esc` sets its cancel flag. The counter is allowed to be
//! incomplete and says so with a `+`, which is the same honesty the design
//! asks of the line count.
//!
//! Keeping the two apart is what makes the memory rule survive: the hit list is
//! capped at [`MAX_HITS`] whatever the file holds, because navigation does not
//! need it and highlighting only needs the hits *on screen*, which
//! [`Matcher::matches_in`] finds in the bytes already read for the row.
//!
//! # The bug every streaming searcher has
//!
//! A match that straddles a chunk boundary is in neither chunk. Every scan here
//! therefore advances by `window - (pattern - 1)` bytes rather than by `window`,
//! so a match of length *n* starting anywhere in the file lies **wholly** inside
//! some window. That is why [`Matcher::overlap`] exists and why
//! [`MAX_PATTERN`] is far below [`super::source::MAX_WINDOW`] - the overlap
//! rule needs the pattern to fit in a window with room to spare, and the type
//! is what guarantees it. Backwards scans use the mirrored rule.
//!
//! Overlapping windows can report the same hit twice; every scan here only ever
//! accepts a hit at an offset it has not already passed.
//!
//! # Regex is deferred, deliberately
//!
//! the design says matches use `grep-regex`, "the same matcher, so behaviour is
//! identical in the viewer and in file search". `grep-regex` is on the table
//! but it belongs to, and [`FindKind::Regex`] is reachable from the toggle and
//! compiles to [`FindError::RegexDeferred`], which names the milestone - the
//! rule for anything out of scope, applied to half a feature rather than to a
//! whole one.
//!
//! v0.6 brought `grep-regex` and the search dialog's regex, and **not** this
//! one, so [`REGEX_MILESTONE`] moved with it rather than becoming false on the
//! day the milestone shipped. The reason it did not come along: every scan
//! here matches over overlapping windows, and the overlap has to be at least
//! as long as the longest possible match or a hit straddling two windows is
//! missed. A literal and a hex pattern have a known length; a regex does not.
//! Delivering it needs the line-oriented path `grep-searcher` provides, which
//! is the own remaining work rather than a switch that can be
//! flipped here.

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc;

use crate::config::QuickSearchCase;
use crate::error::Result;
use crate::input::Milestone;

use super::ViewerId;
use super::decode::TextEncoding;
use super::encoding;
use super::source::{MAX_WINDOW, Source, WindowLen};

/// The longest compiled pattern, in bytes.
///
/// The overlap rule needs the pattern to fit inside one window with room left
/// for progress, and 256 bytes against a 256 KiB window is a thousandfold
/// margin. It is also about as long a literal as a human types into a find bar.
pub const MAX_PATTERN: usize = 256;

/// How many hits the find bar keeps, whatever the file holds.
///
/// The count keeps rising past this; only the offsets stop being remembered.
/// Navigation does not use them (it streams) and highlighting does not use them
/// (it re-matches the visible bytes), so the list is a convenience and is
/// allowed to be a bounded one - the memory rule applies to the
/// find bar as much as to the index.
pub const MAX_HITS: usize = 4_096;

/// The channel depth between the background counter and the UI.
pub const FIND_CHANNEL_DEPTH: usize = 32;

/// How many windows one interactive search step may read before answering
/// "not yet, resume here".
///
/// The same idea as [`super::NAV_READ_BUDGET`]: a keystroke may not turn into a
/// scan of a 40 GB file. 32 windows is 8 MiB, a few milliseconds off a local
/// disk, and [`Found::Budget`] carries the offset to resume from so the caller
/// can continue on the next frame without losing its place.
pub const FIND_READ_BUDGET: u32 = 32;

/// The most match ranges one line reports for highlighting.
///
/// A screen row cannot show more highlights than it has columns; this is well
/// above any terminal and bounds a pathological line all the same.
pub const MAX_LINE_MATCHES: usize = 1_024;

/// The milestone that brings regex search in **the viewer**.
///
///
/// v0.6 brought `grep-regex` and the search dialog's regex; the find bar's
/// overlap rule needs a bounded match length, which a regex does not have, so
/// this moved rather than becoming false. See the module documentation.
pub const REGEX_MILESTONE: Milestone = Milestone::V07;

// ------------------------------------------------------------- the query ----

/// How the text in the find bar is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FindKind {
    /// Plain text in text mode; a hex byte pattern in hex mode when the input
    /// is one. The default, and what "the same bar accepts either text or a
    /// hex byte pattern" means in practice.
    #[default]
    Auto,
    /// Plain text, whatever the mode.
    Text,
    /// A hex byte pattern (`DE AD BE EF`) with `??` wildcards, whatever the
    /// mode.
    Hex,
    /// Regex. Reachable, and refused with the milestone that brings it.
    Regex,
}

impl FindKind {
    /// Every kind, in the order the toggle cycles them.
    pub const ALL: &'static [Self] = &[Self::Auto, Self::Text, Self::Hex, Self::Regex];

    /// The word shown in the find bar.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Text => "text",
            Self::Hex => "hex",
            Self::Regex => "regex",
        }
    }

    /// False for anything this milestone does not implement.
    pub const fn is_available(self) -> bool {
        !matches!(self, Self::Regex)
    }

    /// The next kind the toggle lands on.
    pub const fn next(self) -> Self {
        match self {
            Self::Auto => Self::Text,
            Self::Text => Self::Hex,
            Self::Hex => Self::Regex,
            Self::Regex => Self::Auto,
        }
    }
}

/// Why a pattern could not be compiled.
///
/// Every one of these is shown in the find bar and none of them closes it: a
/// half-typed pattern is the normal state of an incremental search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindError {
    /// Nothing has been typed yet.
    Empty,
    /// The pattern is longer than [`MAX_PATTERN`] bytes once encoded.
    TooLong {
        /// How many bytes it came to.
        bytes: usize,
    },
    /// The input was read as a hex byte pattern and is not one.
    Hex(String),
    /// A character in the pattern cannot be written in the active encoding, so
    /// no run of bytes in the file could ever match it.
    Unencodable {
        /// The offending character.
        ch: char,
        /// The encoding it cannot be written in.
        encoding: &'static str,
    },
    /// Regex is [`REGEX_MILESTONE`]'s.
    RegexDeferred,
}

impl std::fmt::Display for FindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("type to search"),
            Self::TooLong { bytes } => {
                write!(f, "pattern is {bytes} bytes; the limit is {MAX_PATTERN}")
            }
            Self::Hex(what) => write!(f, "{what}"),
            Self::Unencodable { ch, encoding } => {
                write!(f, "{ch:?} cannot be written in {encoding}")
            }
            Self::RegexDeferred => write!(
                f,
                "regex search arrives in {REGEX_MILESTONE} with file search"
            ),
        }
    }
}

/// What compiling a pattern gives back.
///
/// Spelled out rather than reusing [`crate::error::Result`], because a pattern
/// that does not compile is not a crate-level failure - it is the normal state
/// of a find bar with three characters typed into it, and it is shown *in* the
/// bar rather than raised.
pub type Compiled<T> = std::result::Result<T, FindError>;

/// What is in the find bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindQuery {
    /// What has been typed.
    pub input: String,
    /// How to read it.
    pub kind: FindKind,
    /// Case handling. "case-insensitive by default, smart-case
    /// optional", so the default here differs from the panel's,
    /// which defaults to smart.
    pub case: QuickSearchCase,
}

impl Default for FindQuery {
    fn default() -> Self {
        Self {
            input: String::new(),
            kind: FindKind::Auto,
            case: QuickSearchCase::Insensitive,
        }
    }
}

impl FindQuery {
    /// Does this query match case-sensitively?
    ///
    /// The same rule the panel's quick search uses, so `smart` means the same
    /// thing in both places.
    pub fn case_sensitive(&self) -> bool {
        crate::input::quicksearch::is_case_sensitive(&self.input, self.case)
    }

    /// How this query is actually read, given the mode the viewer is in.
    ///
    /// [`FindKind::Auto`] becomes [`FindKind::Hex`] only in hex mode and only
    /// when the input really parses as whole bytes, so `zz` in hex mode is
    /// still a text search rather than an error.
    pub fn effective_kind(&self, hex_mode: bool) -> FindKind {
        match self.kind {
            FindKind::Auto if hex_mode && looks_like_hex_pattern(&self.input) => FindKind::Hex,
            FindKind::Auto => FindKind::Text,
            other => other,
        }
    }
}

// ----------------------------------------------------------- the matcher ----

/// What one byte of a pattern accepts.
///
/// Public since v0.6, because the `Hex` checkbox is "the same
/// syntax as the viewer's hex find" and the only way to make that a code fact
/// rather than a promise is for both to come out of [`hex_classes`].
/// `crate::search::content::hex_regex` translates these into a `grep-regex`
/// pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Exactly this byte.
    Exact(u8),
    /// Either of two bytes - one character's two cases, in an encoding where
    /// they differ in a single byte.
    Either(u8, u8),
    /// Any byte: the `??` wildcard.
    Any,
}

impl Class {
    const fn matches(self, byte: u8) -> bool {
        match self {
            Self::Exact(b) => b == byte,
            Self::Either(a, b) => a == byte || b == byte,
            Self::Any => true,
        }
    }
}

/// A compiled pattern, searched over raw bytes.
///
/// Bytes and not characters, because the search is streamed over the file and
/// the file is bytes. A text pattern is **encoded into the active encoding**
/// when it is compiled, so searching a `cp437` file for `café` looks for the
/// `cp437` bytes of `café` - which is the only reading of "the same bar" that
/// finds anything in a file that is not UTF-8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matcher {
    classes: Vec<Class>,
    /// The pattern as the user typed it, for the bar.
    display: String,
    kind: FindKind,
}

impl Matcher {
    /// Compile the find bar's contents.
    ///
    /// `hex_mode` is the viewer's mode, which is what [`FindKind::Auto`] reads.
    pub fn compile(query: &FindQuery, enc: TextEncoding, hex_mode: bool) -> Compiled<Self> {
        let kind = query.effective_kind(hex_mode);
        if query.input.is_empty() {
            return Err(FindError::Empty);
        }
        let classes = match kind {
            FindKind::Regex => return Err(FindError::RegexDeferred),
            FindKind::Hex => hex_classes(&query.input)?,
            // `Auto` has already been resolved by `effective_kind`.
            FindKind::Text | FindKind::Auto => {
                text_classes(&query.input, enc, !query.case_sensitive())?
            }
        };
        if classes.is_empty() {
            return Err(FindError::Empty);
        }
        if classes.len() > MAX_PATTERN {
            return Err(FindError::TooLong {
                bytes: classes.len(),
            });
        }
        Ok(Self {
            classes,
            display: query.input.clone(),
            kind,
        })
    }

    /// A plain-text matcher, for callers that have no find bar.
    pub fn text(needle: &str, enc: TextEncoding, case_sensitive: bool) -> Compiled<Self> {
        Self::compile(
            &FindQuery {
                input: needle.to_string(),
                kind: FindKind::Text,
                case: if case_sensitive {
                    QuickSearchCase::Sensitive
                } else {
                    QuickSearchCase::Insensitive
                },
            },
            enc,
            false,
        )
    }

    /// A hex byte matcher (`DE AD ?? EF`).
    pub fn hex(pattern: &str) -> Compiled<Self> {
        Self::compile(
            &FindQuery {
                input: pattern.to_string(),
                kind: FindKind::Hex,
                case: QuickSearchCase::Sensitive,
            },
            TextEncoding::UTF8,
            true,
        )
    }

    /// How many bytes a match is.
    pub fn len(&self) -> usize {
        self.classes.len()
    }

    /// True for a pattern with nothing in it, which no constructor produces.
    pub fn is_empty(&self) -> bool {
        self.classes.is_empty()
    }

    /// How the pattern was read.
    pub const fn kind(&self) -> FindKind {
        self.kind
    }

    /// The pattern as typed.
    pub fn display(&self) -> &str {
        &self.display
    }

    /// **The chunk-straddling rule.** Consecutive windows must overlap by this
    /// many bytes for every match in the file to lie wholly inside one of them.
    pub fn overlap(&self) -> usize {
        self.classes.len().saturating_sub(1)
    }

    /// Does a match start exactly at `at`?
    pub fn matches_at(&self, hay: &[u8], at: usize) -> bool {
        let Some(window) = hay.get(at..at.saturating_add(self.classes.len())) else {
            return false;
        };
        self.classes
            .iter()
            .zip(window.iter())
            .all(|(c, b)| c.matches(*b))
    }

    /// The first match at or after `from`.
    pub fn find_in(&self, hay: &[u8], from: usize) -> Option<usize> {
        let len = self.classes.len();
        let last = hay.len().checked_sub(len)?;
        let mut at = from;
        // A leading exact byte turns the outer loop into a byte search, which
        // is the difference between a scan that keeps up with a disk and one
        // that does not.
        let lead = match self.classes.first() {
            Some(Class::Exact(b)) => Some(*b),
            _ => None,
        };
        while at <= last {
            if let Some(b) = lead {
                let rest = hay.get(at..=last)?;
                at = at.saturating_add(rest.iter().position(|x| *x == b)?);
            }
            if self.matches_at(hay, at) {
                return Some(at);
            }
            at = at.saturating_add(1);
        }
        None
    }

    /// The last match starting strictly before `before`.
    pub fn rfind_in(&self, hay: &[u8], before: usize) -> Option<usize> {
        let len = self.classes.len();
        let last = hay.len().checked_sub(len)?;
        let mut at = last.min(before.checked_sub(1)?);
        loop {
            if self.matches_at(hay, at) {
                return Some(at);
            }
            at = at.checked_sub(1)?;
        }
    }

    /// Every non-overlapping match in `hay`, ascending.
    ///
    /// This is what paints `viewer.match` on a row: the bytes are the ones the
    /// layout already read, so highlighting costs no extra I/O and needs no hit
    /// list. Capped at [`MAX_LINE_MATCHES`].
    pub fn matches_in(&self, hay: &[u8]) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        let mut at = 0_usize;
        while out.len() < MAX_LINE_MATCHES {
            let Some(hit) = self.find_in(hay, at) else {
                break;
            };
            let end = hit.saturating_add(self.classes.len());
            out.push(hit..end);
            at = end.max(hit.saturating_add(1));
        }
        out
    }
}

// ------------------------------------------------------- pattern parsing ----

/// Could this input be a hex byte pattern? (the `DE AD BE EF`.)
pub fn looks_like_hex_pattern(input: &str) -> bool {
    hex_classes(input).is_ok()
}

/// `DE AD BE EF`, `deadbeef`, `0xDE,0xAD`, `DE ?? EF`.
///
/// Separators are whitespace, commas and colons; a token may carry an `0x`,
/// `\x` or `$` prefix. `??` is the wildcard, and it is a whole byte because a
/// half-specified byte is not something the design offers.
///
/// **The one parser of this grammar.** The find bar reads it here and so does
/// the Find Files dialog's `Hex` checkbox, through
/// `crate::search::content::hex_regex`, so the two cannot drift.
///
pub fn hex_classes(input: &str) -> Compiled<Vec<Class>> {
    let mut out = Vec::new();
    for token in input
        .split([' ', '\t', ',', ':', '\n'])
        .filter(|t| !t.is_empty())
    {
        let body = token
            .strip_prefix("0x")
            .or_else(|| token.strip_prefix("0X"))
            .or_else(|| token.strip_prefix("\\x"))
            .or_else(|| token.strip_prefix('$'))
            .unwrap_or(token);
        let chars: Vec<char> = body.chars().collect();
        if chars.is_empty() {
            return Err(FindError::Hex(format!("{token:?} is not a hex byte")));
        }
        if !chars.len().is_multiple_of(2) {
            return Err(FindError::Hex(format!(
                "{token:?} is {} hex digits; a byte pattern needs whole bytes",
                chars.len()
            )));
        }
        for pair in chars.chunks(2) {
            let (hi, lo) = match pair {
                [a, b] => (*a, *b),
                _ => return Err(FindError::Hex("a byte pattern needs whole bytes".into())),
            };
            if hi == '?' && lo == '?' {
                out.push(Class::Any);
                continue;
            }
            let (Some(h), Some(l)) = (hi.to_digit(16), lo.to_digit(16)) else {
                return Err(FindError::Hex(format!(
                    "{hi}{lo} is not a hex byte (use ?? for any byte)"
                )));
            };
            let byte = u8::try_from(h.saturating_mul(16).saturating_add(l))
                .map_err(|_| FindError::Hex("a byte pattern needs whole bytes".into()))?;
            out.push(Class::Exact(byte));
        }
        if out.len() > MAX_PATTERN {
            return Err(FindError::TooLong { bytes: out.len() });
        }
    }
    if out.is_empty() {
        return Err(FindError::Empty);
    }
    Ok(out)
}

/// A text needle, encoded into the active encoding, one character at a time.
///
/// Character by character rather than in one call, because case-insensitive
/// matching needs to know which *byte* of a character carries its case.
fn text_classes(needle: &str, enc: TextEncoding, fold: bool) -> Compiled<Vec<Class>> {
    let mut out = Vec::new();
    for ch in needle.chars() {
        out.extend(char_classes(enc, ch, fold)?);
        if out.len() > MAX_PATTERN {
            return Err(FindError::TooLong { bytes: out.len() });
        }
    }
    Ok(out)
}

/// One character's byte classes.
fn char_classes(enc: TextEncoding, ch: char, fold: bool) -> Compiled<Vec<Class>> {
    let exact = encode_char(enc, ch).ok_or(FindError::Unencodable {
        ch,
        encoding: enc.label(),
    })?;
    let literal = || exact.iter().copied().map(Class::Exact).collect::<Vec<_>>();
    if !fold {
        return Ok(literal());
    }
    // A character whose case change is not one character for one - `ß` upper
    // cases to `SS` - has no per-byte answer. Matching it literally is right
    // and is what ripgrep's default does too.
    if ch.to_lowercase().count() != 1 || ch.to_uppercase().count() != 1 {
        return Ok(literal());
    }
    let (Some(lower), Some(upper)) = (ch.to_lowercase().next(), ch.to_uppercase().next()) else {
        return Ok(literal());
    };
    if lower == upper {
        return Ok(literal());
    }
    let (Some(lb), Some(ub)) = (encode_char(enc, lower), encode_char(enc, upper)) else {
        return Ok(literal());
    };
    // A title-case character (`ǅ`) is neither of its own two case forms, so
    // folding it to a pair would stop it matching itself.
    if exact != lb && exact != ub {
        return Ok(literal());
    }
    if lb.len() != ub.len() {
        return Ok(literal());
    }
    let differing: Vec<usize> = lb
        .iter()
        .zip(ub.iter())
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(i, _)| i)
        .collect();
    match differing.as_slice() {
        [] => Ok(lb.iter().copied().map(Class::Exact).collect()),
        [only] => Ok(lb
            .iter()
            .zip(ub.iter())
            .enumerate()
            .map(|(i, (a, b))| {
                if i == *only {
                    Class::Either(*a, *b)
                } else {
                    Class::Exact(*a)
                }
            })
            .collect()),
        // Two cases that differ in more than one byte cannot be expressed as a
        // per-byte class. Literal, and honest about it.
        _ => Ok(literal()),
    }
}

/// One character's bytes in `enc`, or `None` when it has none.
fn encode_char(enc: TextEncoding, ch: char) -> Option<Vec<u8>> {
    match enc {
        TextEncoding::Rs(rs) if rs == encoding_rs::UTF_16LE => Some(utf16_bytes(ch, false)),
        TextEncoding::Rs(rs) if rs == encoding_rs::UTF_16BE => Some(utf16_bytes(ch, true)),
        TextEncoding::Rs(rs) => {
            let mut buf = [0_u8; 4];
            let text: &str = ch.encode_utf8(&mut buf);
            let (bytes, _, unmappable) = rs.encode(text);
            if unmappable {
                return None;
            }
            let mut bytes = bytes.into_owned();
            if rs == encoding_rs::ISO_2022_JP {
                // Encoding one character on its own wraps it in the escape
                // sequences that designate its charset. In the file those
                // escapes were emitted once, further up; what is at the
                // character itself is the bare bytes. Searching for the escapes
                // too would find nothing (the stateful case, seen
                // from the search side).
                bytes = strip_iso2022_escapes(&bytes);
            }
            if bytes.is_empty() { None } else { Some(bytes) }
        }
        TextEncoding::Page(page) => page
            .table
            .iter()
            .position(|t| *t == ch)
            .and_then(|i| u8::try_from(i).ok())
            .map(|b| vec![b]),
    }
}

/// One character as UTF-16 bytes.
///
/// By hand, because `encoding_rs` maps `UTF_16LE.encode()` onto **UTF-8**
/// output - the WHATWG rule for a form submission, and silently the wrong bytes
/// for searching a UTF-16 file.
fn utf16_bytes(ch: char, big_endian: bool) -> Vec<u8> {
    let mut units = [0_u16; 2];
    let encoded = ch.encode_utf16(&mut units);
    encoded
        .iter()
        .flat_map(|u| {
            if big_endian {
                u.to_be_bytes()
            } else {
                u.to_le_bytes()
            }
        })
        .collect()
}

/// Drop ISO-2022-JP's three-byte charset escape sequences.
fn strip_iso2022_escapes(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0_usize;
    while i < bytes.len() {
        if bytes.get(i) == Some(&0x1B) && i.saturating_add(3) <= bytes.len() {
            i = i.saturating_add(3);
            continue;
        }
        if let Some(b) = bytes.get(i) {
            out.push(*b);
        }
        i = i.saturating_add(1);
    }
    out
}

// --------------------------------------------------- the streaming search ---

/// What one bounded search step found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    /// A match starts here.
    Hit(u64),
    /// There is no match in the direction searched.
    None,
    /// The read budget ran out. Resume from this offset; nothing before it
    /// holds a match.
    Budget(u64),
}

/// The window each search step reads. One [`MAX_WINDOW`], less nothing.
fn step_window() -> WindowLen {
    WindowLen::MAX
}

/// The first match at or after `from`, streamed.
///
/// Reads at most `budget` windows and holds exactly one at a time, so this is
/// correct on a file of any size and costs the same on all of them.
///
/// **Chunk straddling**: each window starts `matcher.overlap()` bytes before
/// the previous one ended, so a match lying across a window boundary is wholly
/// inside the next window. That is the invariant this function exists to hold.
pub fn find_forward(
    source: &mut Source,
    matcher: &Matcher,
    from: u64,
    budget: u32,
) -> Result<Found> {
    if matcher.is_empty() {
        return Ok(Found::None);
    }
    let overlap = matcher.overlap() as u64;
    let mut at = from;
    for _ in 0..budget.max(1) {
        let window = source.read_window(at, step_window())?;
        if window.is_empty() {
            return Ok(Found::None);
        }
        if let Some(idx) = matcher.find_in(window.bytes(), 0) {
            let abs = window.at().saturating_add(idx as u64);
            if abs >= from {
                return Ok(Found::Hit(abs));
            }
        }
        if window.hit_eof() {
            return Ok(Found::None);
        }
        // Step back by the overlap, and never fail to make progress: the
        // pattern is capped at MAX_PATTERN and the window at MAX_WINDOW, so
        // the subtraction always leaves room, but the `max` says so locally.
        at = window
            .end()
            .saturating_sub(overlap)
            .max(at.saturating_add(1));
    }
    Ok(Found::Budget(at))
}

/// The last match starting strictly before `before`, streamed backwards.
///
/// The mirror of [`find_forward`]: each window reaches `overlap` bytes past
/// where the previous one began, so a match across a boundary is wholly inside
/// one of them.
pub fn find_backward(
    source: &mut Source,
    matcher: &Matcher,
    before: u64,
    budget: u32,
) -> Result<Found> {
    if matcher.is_empty() || before == 0 {
        return Ok(Found::None);
    }
    let span = MAX_WINDOW as u64;
    let overlap = matcher.overlap() as u64;
    // A match starting at `before - 1` ends `len - 1` bytes later, so the
    // region has to reach that far to see it whole.
    let mut hi = before.saturating_add(overlap);
    for _ in 0..budget.max(1) {
        let lo = hi.saturating_sub(span);
        let want = usize::try_from(hi.saturating_sub(lo)).unwrap_or(MAX_WINDOW);
        if want == 0 {
            return Ok(Found::None);
        }
        let window = source.read_window(lo, WindowLen::new(want))?;
        if !window.is_empty() {
            // Only matches that start before `before` count.
            let limit = usize::try_from(before.saturating_sub(window.at()))
                .unwrap_or(usize::MAX)
                .min(window.len());
            if let Some(idx) = matcher.rfind_in(window.bytes(), limit) {
                return Ok(Found::Hit(window.at().saturating_add(idx as u64)));
            }
        }
        if lo == 0 {
            return Ok(Found::None);
        }
        hi = lo.saturating_add(overlap);
    }
    Ok(Found::Budget(hi))
}

/// The decoded-text byte ranges of the matches inside one line.
///
/// `raw` is the line's bytes as the layout read them, so this needs no I/O at
/// all. The ranges index the **decoded** line, which is the same coordinate
/// system [`super::highlight::Span`] uses, so the renderer maps both onto the
/// expanded text the same way.
///
/// Match boundaries are snapped outwards onto character boundaries first: a hex
/// pattern can land in the middle of a multi-byte character, and half a
/// character is not something a highlight can cover.
pub fn match_ranges_in_line(matcher: &Matcher, enc: TextEncoding, raw: &[u8]) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut raw_at = 0_usize;
    let mut decoded_at = 0_usize;
    for hit in matcher.matches_in(raw) {
        let start = boundary_at_or_before(enc, raw, hit.start).max(raw_at);
        let end = boundary_at_or_after(enc, raw, hit.end).max(start);
        let Some(gap) = raw.get(raw_at..start) else {
            break;
        };
        decoded_at = decoded_at.saturating_add(decoded_len(enc, gap));
        let Some(body) = raw.get(start..end) else {
            break;
        };
        let began = decoded_at;
        decoded_at = decoded_at.saturating_add(decoded_len(enc, body));
        out.push(began..decoded_at);
        raw_at = end;
    }
    out
}

/// How many bytes `bytes` decodes to.
fn decoded_len(enc: TextEncoding, bytes: &[u8]) -> usize {
    encoding::decode_window(enc, bytes, true).text.len()
}

/// The character boundary at or before `at`.
fn boundary_at_or_before(enc: TextEncoding, raw: &[u8], at: usize) -> usize {
    let head = raw.get(..at.min(raw.len())).unwrap_or(raw);
    head.len()
        .saturating_sub(encoding::incomplete_tail(enc, head))
}

/// The character boundary at or after `at`.
fn boundary_at_or_after(enc: TextEncoding, raw: &[u8], at: usize) -> usize {
    let at = at.min(raw.len());
    for j in at..=at.saturating_add(encoding::MAX_TAIL).min(raw.len()) {
        if boundary_at_or_before(enc, raw, j) == j {
            return j;
        }
    }
    at
}

// ------------------------------------------------- the background counter ---

/// One step of the background match count, on its way to the UI
/// (the "a match counter fills in behind the cursor").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindBatch {
    /// Which viewer asked.
    pub id: ViewerId,
    /// Which search asked. Typing one more character starts a new search; a
    /// batch from the previous one is dropped rather than counted, which is the
    /// same generation rule [`crate::app::ReadRequest`] uses for a directory
    /// read.
    pub generation: u64,
    /// Bytes searched so far, from the start of the file.
    pub scanned: u64,
    /// New hit offsets, ascending, all beyond every hit sent before. Empty once
    /// [`MAX_HITS`] have been sent; `total` keeps rising.
    pub hits: Vec<u64>,
    /// How many matches have been found so far.
    pub total: u64,
    /// True when the scan reached the end of the file.
    pub done: bool,
    /// Set when the scan stopped on a read error.
    pub error: Option<String>,
}

/// What the event loop must spawn for a search.
///
/// Built by the viewer, spawned by the event loop, for the same reason
/// [`super::ScanJob`] is: the state machine stays drivable with no runtime.
pub struct FindJob {
    /// Which viewer.
    pub id: ViewerId,
    /// Which search.
    pub generation: u64,
    /// The counter's own cursor over the file.
    pub source: Source,
    /// What to count.
    pub matcher: Matcher,
    /// How much to read between batches.
    pub chunk: u64,
    /// "cancellable with `Esc`".
    pub cancel: Arc<AtomicBool>,
}

impl std::fmt::Debug for FindJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FindJob")
            .field("id", &self.id)
            .field("generation", &self.generation)
            .field("pattern", &self.matcher.display())
            .finish_non_exhaustive()
    }
}

/// The background match counter.
///
/// > A match counter fills in behind the cursor while the scan continues in the
/// > background, cancellable with `Esc`.
///
/// Blocking on purpose - this is `spawn_blocking` work, like
/// [`super::index::scan`]. It holds one window at a time, checks `cancel`
/// between windows, and stops the moment its channel closes.
pub fn scan(
    id: ViewerId,
    generation: u64,
    mut source: Source,
    matcher: Matcher,
    chunk: u64,
    cancel: Arc<AtomicBool>,
    tx: mpsc::Sender<FindBatch>,
) {
    if matcher.is_empty() {
        return;
    }
    let overlap = matcher.overlap() as u64;
    let want = usize::try_from(chunk).unwrap_or(MAX_WINDOW).max(
        // The window has to be strictly longer than the pattern or the overlap
        // rule cannot make progress.
        matcher.len().saturating_mul(4).max(4096),
    );
    let step = WindowLen::new(want);
    let mut at = 0_u64;
    let mut total = 0_u64;
    let mut sent = 0_usize;
    // Every hit at or after this offset is new. Overlapping windows would
    // otherwise report the boundary hits twice.
    let mut accept_from = 0_u64;

    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let window = match source.read_window(at, step) {
            Ok(w) => w,
            Err(err) => {
                let _ = tx.blocking_send(FindBatch {
                    id,
                    generation,
                    scanned: at,
                    hits: Vec::new(),
                    total,
                    done: false,
                    error: Some(err.to_string()),
                });
                return;
            }
        };
        if window.is_empty() {
            let _ = tx.blocking_send(FindBatch {
                id,
                generation,
                scanned: at,
                hits: Vec::new(),
                total,
                done: true,
                error: None,
            });
            return;
        }
        let mut hits = Vec::new();
        let mut i = 0_usize;
        while let Some(idx) = matcher.find_in(window.bytes(), i) {
            let abs = window.at().saturating_add(idx as u64);
            if abs >= accept_from {
                total = total.saturating_add(1);
                if sent < MAX_HITS {
                    hits.push(abs);
                    sent = sent.saturating_add(1);
                }
                accept_from = abs.saturating_add(matcher.len() as u64);
            }
            // Non-overlapping matches, which is what a counter counts.
            i = idx.saturating_add(matcher.len()).max(idx.saturating_add(1));
        }
        let done = window.hit_eof();
        let scanned = if done {
            window.end()
        } else {
            window.end().saturating_sub(overlap)
        };
        if tx
            .blocking_send(FindBatch {
                id,
                generation,
                scanned,
                hits,
                total,
                done,
                error: None,
            })
            .is_err()
        {
            // Nobody is listening any more.
            return;
        }
        if done {
            return;
        }
        at = scanned.max(at.saturating_add(1));
    }
}

// --------------------------------------------------------- the find bar -----

/// The find bar's state.
///
/// Holds what has been typed, the compiled matcher, the current match, and the
/// background counter's findings. It does **not** hold the file: every search
/// is a call taking the viewer's [`Source`], which is what keeps this testable
/// without one and honest about not buffering.
#[derive(Debug, Clone)]
pub struct Find {
    query: FindQuery,
    matcher: Option<Matcher>,
    error: Option<FindError>,
    generation: u64,
    hits: Vec<u64>,
    total: u64,
    scanned: u64,
    complete: bool,
    current: Option<u64>,
    open: bool,
}

impl Default for Find {
    fn default() -> Self {
        Self::new(QuickSearchCase::Insensitive)
    }
}

impl Find {
    /// A closed find bar.
    pub fn new(case: QuickSearchCase) -> Self {
        Self {
            query: FindQuery {
                case,
                ..FindQuery::default()
            },
            matcher: None,
            error: Some(FindError::Empty),
            generation: 0,
            hits: Vec::new(),
            total: 0,
            scanned: 0,
            complete: false,
            current: None,
            open: false,
        }
    }

    /// Is the bar showing? (`F7`, `/`, `Ctrl+F`.)
    pub const fn is_open(&self) -> bool {
        self.open
    }

    /// Open the bar, keeping whatever was searched for last so `F7` `n` works.
    pub const fn show(&mut self) {
        self.open = true;
    }

    /// `Esc` - close the bar and keep the position.
    ///
    /// The matcher and the current match survive, so `n` still steps and the
    /// highlights stay painted. What ends is the *typing*; the caller cancels
    /// the background counter separately, because it owns the flag.
    pub const fn hide(&mut self) {
        self.open = false;
    }

    /// Forget the search entirely.
    pub fn reset(&mut self) {
        let case = self.query.case;
        let open = self.open;
        *self = Self::new(case);
        self.open = open;
    }

    /// What has been typed.
    pub fn input(&self) -> &str {
        &self.query.input
    }

    /// The whole query.
    pub const fn query(&self) -> &FindQuery {
        &self.query
    }

    /// How the input is being read.
    pub const fn kind(&self) -> FindKind {
        self.query.kind
    }

    /// Case handling.
    pub const fn case(&self) -> QuickSearchCase {
        self.query.case
    }

    /// Append a character. Returns true when the query changed and the caller
    /// must recompile and re-search - the "typing searches
    /// immediately".
    pub fn push(&mut self, ch: char) -> bool {
        self.query.input.push(ch);
        self.invalidate();
        true
    }

    /// Delete the last character. False when there was nothing to delete.
    pub fn backspace(&mut self) -> bool {
        if self.query.input.pop().is_none() {
            return false;
        }
        self.invalidate();
        true
    }

    /// Replace the whole query - the input, how it is read, and its case rule.
    ///
    /// What the "the hit already highlighted" needs: a search
    /// result's viewer is handed the pattern that found it rather than having
    /// it retyped a character at a time. The caller still calls
    /// [`Find::compile`] afterwards, exactly as it would after a keystroke.
    pub fn set_query(&mut self, query: FindQuery) {
        self.query = query;
        self.invalidate();
    }

    /// Replace the whole input.
    pub fn set_input(&mut self, input: impl Into<String>) {
        self.query.input = input.into();
        self.invalidate();
    }

    /// Choose how the input is read.
    pub fn set_kind(&mut self, kind: FindKind) {
        if self.query.kind != kind {
            self.query.kind = kind;
            self.invalidate();
        }
    }

    /// The toggle the design asks for: plain text, hex bytes, or regex -
    /// which says which milestone brings it.
    pub fn toggle_kind(&mut self) {
        self.set_kind(self.query.kind.next());
    }

    /// Cycle case handling: insensitive, sensitive, smart (the design's
    /// "case-insensitive by default, smart-case optional").
    pub fn cycle_case(&mut self) {
        self.query.case = match self.query.case {
            QuickSearchCase::Insensitive => QuickSearchCase::Sensitive,
            QuickSearchCase::Sensitive => QuickSearchCase::Smart,
            QuickSearchCase::Smart => QuickSearchCase::Insensitive,
        };
        self.invalidate();
    }

    /// Compile the query against the active encoding and mode.
    ///
    /// Call this after any change the caller was told about. It compiles and
    /// **nothing else**: the generation, the hits and the count all survive,
    /// which is right after a keystroke (they were cleared when the keystroke
    /// invalidated them) and wrong after a change to what the same characters
    /// *mean*. That is what [`Find::recompile`] is for.
    pub fn compile(&mut self, enc: TextEncoding, hex_mode: bool) {
        match Matcher::compile(&self.query, enc, hex_mode) {
            Ok(matcher) => {
                self.matcher = Some(matcher);
                self.error = None;
            }
            Err(err) => {
                self.matcher = None;
                self.error = Some(err);
            }
        }
    }

    /// Read the same query in a different way, and throw away what the
    /// previous reading counted.
    ///
    /// `F4` in hex mode makes `dead` the bytes `DE AD`; `F8` makes `café`
    /// different bytes. Neither changes a character of the input and both
    /// change every match in the file, so the generation goes up - a counter
    /// still running for the old reading stops being believed - and the hits,
    /// the total and the current match all go with it.
    pub fn recompile(&mut self, enc: TextEncoding, hex_mode: bool) {
        self.invalidate();
        self.current = None;
        self.compile(enc, hex_mode);
    }

    /// The compiled matcher, when the query is valid.
    pub const fn matcher(&self) -> Option<&Matcher> {
        self.matcher.as_ref()
    }

    /// Why the query does not compile, when it does not.
    pub const fn error(&self) -> Option<&FindError> {
        self.error.as_ref()
    }

    /// Which search this is. Batches carrying any other generation are stale.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Fold in one background batch. False when it belongs to another search.
    pub fn apply(&mut self, batch: &FindBatch) -> bool {
        if batch.generation != self.generation {
            return false;
        }
        for hit in &batch.hits {
            if self.hits.len() >= MAX_HITS {
                break;
            }
            if self.hits.last().is_none_or(|last| *last < *hit) {
                self.hits.push(*hit);
            }
        }
        self.scanned = self.scanned.max(batch.scanned);
        self.total = self.total.max(batch.total);
        self.complete |= batch.done;
        true
    }

    /// Where the current match is.
    pub const fn current(&self) -> Option<u64> {
        self.current
    }

    /// Put the cursor on a match.
    pub const fn set_current(&mut self, at: Option<u64>) {
        self.current = at;
    }

    /// The hits the counter has remembered, ascending and capped.
    pub fn hits(&self) -> &[u64] {
        &self.hits
    }

    /// How many matches have been counted so far.
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// How much of the file the counter has been over.
    pub const fn scanned(&self) -> u64 {
        self.scanned
    }

    /// True once the counter has been over the whole file, so `total` is final.
    pub const fn is_complete(&self) -> bool {
        self.complete
    }

    // ------------------------------------------------------ searching ------

    /// One keystroke's worth of search.
    ///
    /// > typing searches immediately, incrementally, with the first match
    /// > highlighted as you type.
    ///
    /// Recompiles the query and puts the current match on the first hit at or
    /// after `from`, which the caller passes as the position the bar opened on
    /// so that an incremental search walks *forward from where you were* rather
    /// than restarting at the top of the file on every character.
    ///
    /// Bounded by `budget` windows. A [`Found::Budget`] answer is not a
    /// failure: it means "not in the first few megabytes", and the caller may
    /// call again with the offset it carries.
    pub fn search(
        &mut self,
        source: &mut Source,
        enc: TextEncoding,
        hex_mode: bool,
        from: u64,
        budget: u32,
    ) -> Result<Found> {
        self.compile(enc, hex_mode);
        let Some(matcher) = self.matcher.as_ref() else {
            self.current = None;
            return Ok(Found::None);
        };
        let found = find_forward(source, matcher, from, budget)?;
        self.current = match found {
            Found::Hit(at) => Some(at),
            Found::None | Found::Budget(_) => None,
        };
        Ok(found)
    }

    /// `n` - the next match after the current one.
    ///
    /// With no current match it starts from `cursor`, so `F7` `Esc` `n` picks
    /// up from where the viewer is rather than from the top of the file.
    ///
    /// **It does not wrap.** the design does not say whether `n` at the last
    /// match returns to the first, and on a file whose match count is still
    /// filling in there is no way to know that this *is* the last match without
    /// finishing the scan - which is the one thing the design forbids
    /// blocking on. So the honest answer at the end is [`Found::None`], and a
    /// caller that wants a wrap can ask again from zero.
    pub fn next_match(&mut self, source: &mut Source, cursor: u64, budget: u32) -> Result<Found> {
        let Some(matcher) = self.matcher.as_ref() else {
            return Ok(Found::None);
        };
        let from = self.current.map_or(cursor, |at| at.saturating_add(1));
        let found = find_forward(source, matcher, from, budget)?;
        if let Found::Hit(at) = found {
            self.current = Some(at);
        }
        Ok(found)
    }

    /// `Shift+N` - the previous match. The mirror of [`Find::next_match`].
    pub fn prev_match(&mut self, source: &mut Source, cursor: u64, budget: u32) -> Result<Found> {
        let Some(matcher) = self.matcher.as_ref() else {
            return Ok(Found::None);
        };
        let before = self.current.unwrap_or(cursor);
        let found = find_backward(source, matcher, before, budget)?;
        if let Found::Hit(at) = found {
            self.current = Some(at);
        }
        Ok(found)
    }

    /// The background counter this search still needs, if any.
    ///
    /// The caller owns the [`Source`] and the cancel flag, so it builds the job
    /// and spawns it; this is only the part that knows *whether* one is owed
    /// and which generation it belongs to.
    pub fn job(
        &self,
        id: ViewerId,
        source: Source,
        chunk: u64,
        cancel: Arc<AtomicBool>,
    ) -> Option<FindJob> {
        Some(FindJob {
            id,
            generation: self.generation,
            source,
            matcher: self.matcher.clone()?,
            chunk,
            cancel,
        })
    }

    /// Which match the cursor is on, 1-based, when it is known.
    ///
    /// Answered from the remembered hits, so it is `None` past [`MAX_HITS`] -
    /// at which point the honest thing to show is the total alone rather than
    /// a number that would be a guess.
    pub fn match_number(&self) -> Option<u64> {
        let at = self.current?;
        let before = self.hits.partition_point(|h| *h < at);
        if before >= self.hits.len() && self.hits.len() >= MAX_HITS {
            return None;
        }
        self.hits
            .get(before)
            .filter(|h| **h == at)
            .map(|_| (before as u64).saturating_add(1))
    }

    /// The counter, as the bar shows it: `3/57`, `3/57+` while still scanning,
    /// `0` when there is nothing to find.
    ///
    /// The `+` is the honesty marker applied to the match count: a
    /// number that is still rising must not look like a final one.
    pub fn counter_text(&self) -> Option<String> {
        self.matcher.as_ref()?;
        let more = if self.complete { "" } else { "+" };
        Some(match (self.match_number(), self.total) {
            (_, 0) if self.complete => "no matches".to_string(),
            (None, n) => format!("{n}{more}"),
            (Some(k), n) => format!("{k}/{n}{more}"),
        })
    }

    /// The whole find bar, as one line (the shape, the content).
    ///
    /// `find: café [aa] 3/57+`, or the error where the counter would be - a
    /// half-typed hex pattern says so rather than showing a stale count.
    pub fn bar_text(&self) -> String {
        self.bar_text_with(self.counter_text())
    }

    /// [`Find::bar_text`] with the counter supplied by the caller.
    ///
    /// Mode 3 counts its own hits - it searches the rendered document in one
    /// pass, so its count is exact and never wears the `+` that says a
    /// background scan is still running. Everything else about the bar is the
    /// same in every mode, and is written once.
    pub fn bar_text_with(&self, counter: Option<String>) -> String {
        let mut out = String::from("find: ");
        out.push_str(&self.query.input);
        out.push(' ');
        out.push_str(crate::input::case_indicator(
            &self.query.input,
            self.query.case,
        ));
        // The kind shown is the **effective** one, for the same reason
        // the design shows the effective case mode rather than the configured
        // one: in hex mode `dead` is being read as four bytes, and a bar that
        // did not say so would make a surprising result a mystery. Plain text
        // is the unremarkable case and is not labelled.
        let shown = self.matcher.as_ref().map_or(self.query.kind, Matcher::kind);
        if matches!(shown, FindKind::Hex | FindKind::Regex) {
            out.push_str(" [");
            out.push_str(shown.label());
            out.push(']');
        }
        match (&self.error, counter) {
            (Some(FindError::Empty), _) => {}
            (Some(err), _) => {
                out.push_str("  ");
                out.push_str(&err.to_string());
            }
            (None, Some(counter)) => {
                out.push_str("  ");
                out.push_str(&counter);
            }
            (None, None) => {}
        }
        out
    }

    /// Everything a changed query invalidates.
    fn invalidate(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.matcher = None;
        self.error = Some(FindError::Empty);
        self.hits.clear();
        self.total = 0;
        self.scanned = 0;
        self.complete = false;
    }
}

#[cfg(test)]
mod tests;
