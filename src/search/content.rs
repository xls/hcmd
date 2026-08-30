//! The file-contents half of a search.
//!
//! > **Content**: `grep_searcher::Searcher` with `grep_regex::RegexMatcher`,
//! > streaming per file with binary detection and encoding handling built in.
//!
//! One [`ContentMatcher`] is built per search and shared, unchanged, across
//! every walker thread. It is the only thing in the crate that searches a
//! file's bytes, so "the same `grep-regex` matcher that found it"
//! is a fact about the code and not a promise about two
//! implementations.
//!
//! # Charsets
//!
//! the design makes the charsets independent checkboxes "because a tree can
//! hold files in several encodings", and each selected one is tried per file.
//! Three of the four are `grep_searcher`'s own `encoding()`, which transcodes
//! the stream on the way past; CP437 is not, because `encoding_rs` does not
//! implement the DOS code pages and a `grep_searcher::Encoding` can only name
//! an `encoding_rs` label. So [`Cp437Reader`] transcodes that one here, from
//! the table `crate::viewer::decode` already carries.
//!
//!
//! **A hit's offset is the start of the matching line**, in the *decoded*
//! stream. For UTF-8 - the default and the overwhelming majority - decoding is
//! the identity and that is a byte offset into the file, which is what
//! the design makes a position. For the other three charsets the file's own
//! byte offset differs, and the line number, which decoding preserves exactly,
//! is the reliable half.
//!
//! Which of the two a hit carries is [`ContentHit::decoded`], and it is a
//! field rather than a convention because the viewer has to seek by one or the
//! other and cannot guess: handing it a UTF-16 hit's decoded offset opened the
//! file roughly half way to the line the status bar had just named. So the
//! viewer opens at the offset when it is the file's and at the line when it is
//! not ([`crate::viewer::HitStart`]), and the find query it is handed is what
//! highlights the hit inside the line either way.
//!
//! # Why a positive and a negative search both stop at the first hit
//!
//! the design says the inverse match "needs the searcher to run to
//! completion per file rather than stopping at the first hit". What that means
//! is that **absence** cannot be concluded early: a hit disqualifies the file
//! and there is nothing further to learn from the rest of it, so the search
//! stops - it just stops with the answer "no". The cost the design is warning
//! about is real and unavoidable: a file that *qualifies* for an inverted
//! search has been read in full, in every selected charset.
//!
//! # Lines and byte patterns
//!
//! The searcher is line-oriented, exactly as ripgrep is without `-U`: a
//! pattern cannot match across a line break. That is the right default for
//! text and the wrong one for a byte pattern, so a hex pattern that **can**
//! match a line terminator - an explicit `0A` or a `??` wildcard - switches the
//! searcher to multi-line, which reads the file into memory rather than
//! streaming it. Such a search is therefore capped at [`MAX_MULTILINE_BYTES`]
//! rather than at [`MAX_CONTENT_BYTES`]: `grep_searcher` can only avoid that
//! read by memory-mapping the file, and mapping is an `unsafe` call this crate
//! forbids at the top of `lib.rs`.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkFinish, SinkMatch};

use crate::error::{Error, Result};
use crate::vfs::{ContentHit, MAX_HIT_LINE};
use crate::viewer::find::Class;

use super::query::{Charset, ContentQuery, MAX_PATTERN_BYTES, TextMode};

/// How much of one file a content search reads before giving up on it.
///
/// Not a configuration key: the design makes the generated reference file a
/// promise about the defaults and the design introduces no key for this. 1 GiB
/// is far above any file a person searches the text of and far below a runaway.
pub const MAX_CONTENT_BYTES: u64 = 1024 * 1024 * 1024;

/// How much of one file a **multi-line** search reads - see the module docs.
///
/// A multi-line search holds the whole file in memory at once, so this is a
/// memory budget rather than a patience budget, and it is two orders of
/// magnitude below the streaming one for that reason.
pub const MAX_MULTILINE_BYTES: u64 = 64 * 1024 * 1024;

/// How many source bytes [`Cp437Reader`] transcodes at a time.
const CP437_CHUNK: usize = 8 * 1024;

/// The matcher for one content query, over every selected charset.
///
/// `Send + Sync`: one of these is built per search and shared across every
/// walker thread. The [`Searcher`] is *not* part of it - a searcher owns
/// mutable buffers and is built per file, which is where ripgrep builds one
/// too.
#[derive(Debug)]
pub struct ContentMatcher {
    matcher: grep_regex::RegexMatcher,
    charsets: Vec<Charset>,
    inverted: bool,
    /// A byte pattern: binary detection off, because a byte pattern is
    /// precisely a question about a binary file.
    hex: bool,
    /// The pattern can match a line terminator, so the search cannot be
    /// line-oriented. See the module docs.
    multi_line: bool,
}

impl ContentMatcher {
    /// Build it.
    ///
    /// Fails on a pattern `grep_regex` refuses, on an unparseable hex pattern,
    /// on a pattern longer than [`MAX_PATTERN_BYTES`], on an **empty** pattern
    /// and on a query with no charset selected. Those last two are the two
    /// ways a ticked "Find text" box can be meaningless, and the design's
    /// checkbox rule is that it is refused rather than quietly downgraded to a
    /// name-only search.
    pub fn compile(q: &ContentQuery) -> Result<Self> {
        if q.pattern.is_empty() {
            return Err(Error::msg("a content search needs a pattern"));
        }
        if q.pattern.len() > MAX_PATTERN_BYTES {
            return Err(Error::msg(format!(
                "the search text is longer than {MAX_PATTERN_BYTES} bytes"
            )));
        }
        if !q.charsets.any() {
            return Err(Error::msg(
                "a content search needs at least one charset ticked",
            ));
        }

        let mut builder = grep_regex::RegexMatcherBuilder::new();
        let (pattern, hex, multi_line) = match q.mode {
            TextMode::Plain => {
                // A literal, escaped by the matcher itself rather than by a
                // second escaper here: `a.c` finds `a.c` and not `abc`.
                builder.fixed_strings(true);
                (q.pattern.clone(), false, false)
            }
            TextMode::Regex => (q.pattern.clone(), false, false),
            TextMode::Hex => {
                // One grammar, two engines: the viewer's find bar parses it and
                // this translates the result.
                let classes = crate::viewer::find::hex_classes(&q.pattern)
                    .map_err(|e| Error::msg(e.to_string()))?;
                let multi_line = hex_spans_lines(&classes);
                (hex_regex(&classes), true, multi_line)
            }
        };
        if !hex {
            // Case and word boundaries are questions about text. A byte
            // pattern has neither, and applying them to `\xDE` would change
            // which bytes it matched.
            builder.case_insensitive(!q.case_sensitive);
            builder.word(q.whole_words);
        }
        let matcher = builder
            .build(&pattern)
            .map_err(|e| Error::msg(format!("{:?} cannot be searched for: {e}", q.pattern)))?;

        Ok(Self {
            matcher,
            charsets: q.charsets.selected(),
            inverted: q.inverted,
            hex,
            multi_line,
        })
    }

    /// The charsets it will try, in [`super::query::Charsets::selected`]'s
    /// order.
    pub fn charsets(&self) -> &[Charset] {
        &self.charsets
    }

    /// Whether a hit disqualifies rather than qualifies (the design's
    /// "Find files NOT containing the text").
    pub const fn inverted(&self) -> bool {
        self.inverted
    }

    /// The largest file this matcher will read.
    ///
    /// [`MAX_CONTENT_BYTES`] for a streaming search and
    /// [`MAX_MULTILINE_BYTES`] for one that has to hold the file in memory -
    /// see the module docs. Public because the walker has the file's size in
    /// hand already and can skip it for one comparison rather than for an open
    /// and a `stat`.
    pub const fn max_bytes(&self) -> u64 {
        if self.multi_line {
            MAX_MULTILINE_BYTES
        } else {
            MAX_CONTENT_BYTES
        }
    }

    /// Search one file.
    ///
    /// **Blocking.** Called from the walker's own threads, which is where
    /// ripgrep does the same work.
    ///
    /// The file is opened once and rewound between charsets, so a search in
    /// four charsets is one `open` and four reads rather than four opens.
    pub fn search(&self, path: &Path) -> Outcome {
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(err) => return Outcome::Skipped(format!("{}: {err}", path.display())),
        };
        let cap = if self.multi_line {
            MAX_MULTILINE_BYTES
        } else {
            MAX_CONTENT_BYTES
        };
        match file.metadata() {
            Ok(meta) if meta.len() > cap => {
                return Outcome::Skipped(format!(
                    "{}: {} bytes is past the {cap}-byte search limit",
                    path.display(),
                    meta.len()
                ));
            }
            Ok(_) => {}
            Err(err) => return Outcome::Skipped(format!("{}: {err}", path.display())),
        }

        let mut binary = false;
        for (index, charset) in self.charsets.iter().enumerate() {
            // Every charset after the first reads the same bytes again, so the
            // handle goes back to the start rather than being reopened.
            if index > 0
                && let Err(err) = file.seek(SeekFrom::Start(0))
            {
                return Outcome::Skipped(format!("{}: {err}", path.display()));
            }
            match self.search_charset(&mut file, *charset, &path.to_string_lossy()) {
                Outcome::Match(hit) => return self.qualify(hit),
                Outcome::NoMatch => {}
                // Binary in one charset is binary in all of them: the bytes
                // are the same bytes.
                Outcome::Binary => {
                    binary = true;
                    break;
                }
                Outcome::Skipped(why) => return Outcome::Skipped(why),
            }
        }
        if binary {
            // Absence cannot be concluded from a file the searcher refused to
            // read to the end, so an inverted search does not claim it either.
            return Outcome::Binary;
        }
        self.disqualify()
    }

    /// The same over any reader, for an archive member or a remote file
    /// (the last bullet).
    ///
    /// A reader cannot be rewound, so this searches the **first** selected
    /// charset only. A caller that can reopen the stream - which every
    /// [`crate::vfs::Vfs`] can, through `open_read` - walks [`Self::charsets`]
    /// and calls [`Self::search_charset`] per charset instead, which is what
    /// [`super::backend::walk`] does.
    pub fn search_reader(&self, reader: &mut dyn Read, label: &str) -> Outcome {
        let Some(charset) = self.charsets.first().copied() else {
            return Outcome::NoMatch;
        };
        match self.search_charset(reader, charset, label) {
            Outcome::Match(hit) => self.qualify(hit),
            Outcome::NoMatch => self.disqualify(),
            other => other,
        }
    }

    /// Search one reader in one charset, with no inversion applied.
    ///
    /// [`Outcome::Match`] here means "the text is in this stream", whatever
    /// the query's inversion says; the two public entry points are what turn
    /// that into a qualification or a disqualification. Split out so that a
    /// caller which can reopen its stream can try every charset without this
    /// type having to know how ("Each selected charset is tried
    /// per file").
    pub fn search_charset(&self, reader: &mut dyn Read, charset: Charset, label: &str) -> Outcome {
        let mut builder = SearcherBuilder::new();
        builder
            .line_number(true)
            .multi_line(self.multi_line)
            .binary_detection(if self.hex {
                // A byte pattern is a question about binary files.
                BinaryDetection::none()
            } else {
                // ripgrep's own default, and what makes a text search of a
                // source tree survive a `target/` directory.
                BinaryDetection::quit(b'\x00')
            });
        if let Some(encoding_label) = charset.encoding_label() {
            match grep_searcher::Encoding::new(encoding_label) {
                Ok(encoding) => {
                    builder.encoding(Some(encoding));
                }
                Err(err) => return Outcome::Skipped(format!("{label}: {encoding_label}: {err}")),
            }
        }
        let mut searcher = builder.build();
        let mut sink = FirstHit::new(charset);

        let result = match charset {
            // `encoding_rs` has no DOS code page, so this one is transcoded on
            // the way in rather than by the searcher.
            Charset::Cp437 => {
                let mut reader = Cp437Reader::new(reader);
                searcher.search_reader(&self.matcher, &mut reader, &mut sink)
            }
            Charset::Utf8 | Charset::Utf16 | Charset::Latin1 => {
                searcher.search_reader(&self.matcher, reader, &mut sink)
            }
        };
        match result {
            Ok(()) => {}
            Err(err) => return Outcome::Skipped(format!("{label}: {err}")),
        }
        match sink.hit {
            Some(hit) => Outcome::Match(Some(hit)),
            None if sink.binary.is_some() => Outcome::Binary,
            None => Outcome::NoMatch,
        }
    }

    /// What a hit means, given the inversion.
    fn qualify(&self, hit: Option<Box<ContentHit>>) -> Outcome {
        if self.inverted {
            Outcome::NoMatch
        } else {
            Outcome::Match(hit)
        }
    }

    /// What no hit anywhere in the file means, given the inversion.
    ///
    /// An inverted search has no hit to point at, which is why
    /// [`Outcome::Match`] carries an `Option` rather than a `ContentHit`.
    fn disqualify(&self) -> Outcome {
        if self.inverted {
            Outcome::Match(None)
        } else {
            Outcome::NoMatch
        }
    }
}

/// What one file's content search came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// It qualifies. `hit` is `Some` for a positive search and **`None` for an
    /// inverted one**, which has no hit to point at.
    Match(Option<Box<ContentHit>>),
    /// It does not qualify.
    NoMatch,
    /// Binary, and the search was not a hex one, so it was skipped.
    Binary,
    /// Longer than [`MAX_CONTENT_BYTES`], or unreadable. Counted and reported
    /// once at the end rather than per file (the honesty rule
    /// applied to a walk).
    Skipped(String),
}

// ------------------------------------------------------------ the sink ----

/// The first hit in one file, and nothing after it.
#[derive(Debug)]
struct FirstHit {
    charset: &'static str,
    /// Whether this charset's stream was transcoded, and therefore whether the
    /// sink's byte offsets are the file's ([`ContentHit::decoded`]).
    decoded: bool,
    hit: Option<Box<ContentHit>>,
    /// Where the searcher decided the file was binary, when it did.
    binary: Option<u64>,
}

impl FirstHit {
    fn new(charset: Charset) -> Self {
        Self {
            charset: charset.label(),
            decoded: charset.is_transcoded(),
            hit: None,
            binary: None,
        }
    }
}

impl Sink for FirstHit {
    type Error = io::Error;

    fn matched(
        &mut self,
        _searcher: &Searcher,
        mat: &SinkMatch<'_>,
    ) -> std::result::Result<bool, Self::Error> {
        self.hit = Some(Box::new(ContentHit {
            offset: mat.absolute_byte_offset(),
            decoded: self.decoded,
            line: mat.line_number(),
            line_text: crop_line(mat.bytes()),
            charset: self.charset,
        }));
        // `false` stops the search. One hit is the whole question, for a
        // positive search and for an inverted one alike - see the module docs.
        Ok(false)
    }

    fn finish(
        &mut self,
        _searcher: &Searcher,
        finish: &SinkFinish,
    ) -> std::result::Result<(), Self::Error> {
        self.binary = finish.binary_byte_offset();
        Ok(())
    }
}

/// One matched line, decoded lossily, without its terminator, cropped to
/// [`MAX_HIT_LINE`] characters.
///
/// Lossily because a line that decoded to something invalid is still worth
/// showing, and characters rather than bytes because the crop is for a column
/// that counts cells.
fn crop_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim_end_matches(['\n', '\r']);
    let mut out = String::new();
    for (i, ch) in trimmed.chars().enumerate() {
        if i >= MAX_HIT_LINE {
            break;
        }
        out.push(ch);
    }
    out
}

// ---------------------------------------------------------- hex patterns ----

/// `DE AD ?? EF` as a `grep_regex` pattern.
///
/// The classes come from [`crate::viewer::find::hex_classes`], so the viewer's
/// find bar and the `Hex` checkbox parse one grammar. The
/// translation is `(?-u)` - byte mode, so `\xDE` is one byte and not a
/// character - then `\xNN` per exact byte, `[\xNN\xNN]` per two-way class and
/// `(?s:.)` per `??`, which matches any byte including a line terminator.
pub fn hex_regex(classes: &[Class]) -> String {
    let mut out = String::from("(?-u)");
    for class in classes {
        match class {
            Class::Exact(byte) => out.push_str(&escape_byte(*byte)),
            Class::Either(a, b) => {
                out.push('[');
                out.push_str(&escape_byte(*a));
                out.push_str(&escape_byte(*b));
                out.push(']');
            }
            Class::Any => out.push_str("(?s:.)"),
        }
    }
    out
}

/// One byte as `\xNN`, which is exact in byte mode for every value.
fn escape_byte(byte: u8) -> String {
    format!("\\x{byte:02X}")
}

/// Can this pattern match a line terminator?
///
/// A byte pattern that can is not a line-oriented question and the searcher
/// has to be told so - see the module docs.
fn hex_spans_lines(classes: &[Class]) -> bool {
    classes.iter().any(|class| match class {
        Class::Exact(byte) => *byte == b'\n',
        Class::Either(a, b) => *a == b'\n' || *b == b'\n',
        Class::Any => true,
    })
}

// ------------------------------------------------------------- cp437 ----

/// CP437 decoded into UTF-8 on the way past.
///
/// `encoding_rs` does not implement the DOS code pages, which is why
/// `crate::viewer::decode` carries CP437's table in-tree,
/// and `grep_searcher::Encoding` can only name
/// an `encoding_rs` label. So this one charset is transcoded here rather than
/// by the searcher, and the other three are the searcher's own `encoding()`.
///
/// Streaming: it holds one chunk of the source and its transcoding, never the
/// file.
#[derive(Debug)]
pub struct Cp437Reader<R> {
    inner: R,
    /// The transcoded bytes not yet handed out.
    out: Vec<u8>,
    /// How much of `out` has been handed out.
    pos: usize,
    /// True once the source has ended, so a reader that returns `Ok(0)` once
    /// is not asked again.
    done: bool,
}

impl<R: Read> Cp437Reader<R> {
    /// Wrap a reader of CP437 bytes.
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            // Three bytes per source byte is the widest CP437 transcodes to.
            out: Vec::with_capacity(CP437_CHUNK * 3),
            pos: 0,
            done: false,
        }
    }

    /// Refill `out` from one chunk of the source. `Ok(false)` at end of input.
    fn refill(&mut self) -> io::Result<bool> {
        let mut src = [0u8; CP437_CHUNK];
        let read = loop {
            match self.inner.read(&mut src) {
                Ok(0) => {
                    self.done = true;
                    return Ok(false);
                }
                Ok(n) => break n,
                // A short read of zero bytes is not an end of file; an
                // interrupted one is not a failure either.
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        };
        self.out.clear();
        self.pos = 0;
        let table = crate::viewer::decode::CP437.table;
        let mut buf = [0u8; 4];
        for byte in src.get(..read).unwrap_or(&[]) {
            // `table` is 256 long and `byte` is a `u8`, so this cannot miss;
            // it is written as a `get` because nothing in this crate indexes a
            // slice it has not proved.
            let ch = table
                .get(usize::from(*byte))
                .copied()
                .unwrap_or(char::REPLACEMENT_CHARACTER);
            self.out
                .extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        }
        Ok(true)
    }
}

impl<R: Read> Read for Cp437Reader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.pos >= self.out.len() {
            if self.done || !self.refill()? {
                return Ok(0);
            }
        }
        let rest = self.out.get(self.pos..).unwrap_or(&[]);
        let take = rest.len().min(buf.len());
        let Some(target) = buf.get_mut(..take) else {
            return Ok(0);
        };
        let Some(source) = rest.get(..take) else {
            return Ok(0);
        };
        target.copy_from_slice(source);
        self.pos = self.pos.saturating_add(take);
        Ok(take)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::query::Charsets;
    use std::io::Cursor;

    fn query(pattern: &str) -> ContentQuery {
        ContentQuery {
            pattern: pattern.to_string(),
            ..ContentQuery::default()
        }
    }

    fn search_bytes(matcher: &ContentMatcher, bytes: &[u8]) -> Outcome {
        let mut cursor = Cursor::new(bytes.to_vec());
        matcher.search_reader(&mut cursor, "test")
    }

    #[test]
    fn a_plain_pattern_is_a_literal_and_not_a_regex() {
        let m = ContentMatcher::compile(&query("a.c")).expect("plain");
        assert!(matches!(
            search_bytes(&m, b"a.c\n"),
            Outcome::Match(Some(_))
        ));
        assert!(matches!(search_bytes(&m, b"abc\n"), Outcome::NoMatch));
    }

    #[test]
    fn a_hit_carries_its_line_its_number_and_its_charset() {
        let m = ContentMatcher::compile(&query("TODO")).expect("plain");
        let Outcome::Match(Some(hit)) = search_bytes(&m, b"one\ntwo TODO three\nfour\n") else {
            panic!("expected a hit");
        };
        assert_eq!(hit.line, Some(2));
        assert_eq!(hit.offset, 4, "the start of the matching line");
        assert_eq!(hit.line_text, "two TODO three");
        assert_eq!(hit.charset, "UTF-8");
    }

    #[test]
    fn a_very_long_line_is_cropped_on_the_row() {
        let mut body = "x".repeat(MAX_HIT_LINE * 2);
        body.push_str("TODO\n");
        let m = ContentMatcher::compile(&query("TODO")).expect("plain");
        let Outcome::Match(Some(hit)) = search_bytes(&m, body.as_bytes()) else {
            panic!("expected a hit");
        };
        assert_eq!(hit.line_text.chars().count(), MAX_HIT_LINE);
    }

    #[test]
    fn case_and_whole_words_are_the_two_text_toggles() {
        // Insensitive by default, which is the unticked box.
        let m = ContentMatcher::compile(&query("todo")).expect("plain");
        assert!(matches!(search_bytes(&m, b"TODO\n"), Outcome::Match(_)));

        let m = ContentMatcher::compile(&ContentQuery {
            case_sensitive: true,
            ..query("todo")
        })
        .expect("plain");
        assert!(matches!(search_bytes(&m, b"TODO\n"), Outcome::NoMatch));

        let m = ContentMatcher::compile(&ContentQuery {
            whole_words: true,
            ..query("todo")
        })
        .expect("plain");
        assert!(matches!(search_bytes(&m, b"xtodox\n"), Outcome::NoMatch));
        assert!(matches!(search_bytes(&m, b"a todo b\n"), Outcome::Match(_)));
    }

    #[test]
    fn an_inverted_search_reports_the_files_without_the_text() {
        let m = ContentMatcher::compile(&ContentQuery {
            inverted: true,
            ..query("TODO")
        })
        .expect("inverted");
        assert!(m.inverted());

        // Absent: it qualifies, and there is no hit to point at.
        assert_eq!(search_bytes(&m, b"nothing here\n"), Outcome::Match(None));
        // Present: it does not qualify.
        assert_eq!(search_bytes(&m, b"a TODO b\n"), Outcome::NoMatch);
    }

    #[test]
    fn a_file_is_not_reported_as_lacking_text_it_has_at_the_end() {
        // The invariant behind the "run to completion per file":
        // absence is only concluded after the whole file, so a match in the
        // last line disqualifies it just as one in the first does.
        let m = ContentMatcher::compile(&ContentQuery {
            inverted: true,
            ..query("needle")
        })
        .expect("inverted");
        let mut body = "filler\n".repeat(20_000);
        body.push_str("needle\n");
        assert_eq!(search_bytes(&m, body.as_bytes()), Outcome::NoMatch);

        // And one with no needle anywhere still qualifies, however long.
        let body = "filler\n".repeat(20_000);
        assert_eq!(search_bytes(&m, body.as_bytes()), Outcome::Match(None));
    }

    #[test]
    fn a_binary_file_is_skipped_unless_the_pattern_is_bytes() {
        let m = ContentMatcher::compile(&query("TODO")).expect("plain");
        assert_eq!(search_bytes(&m, b"head\x00 TODO\n"), Outcome::Binary);

        // A binary file cannot be reported as lacking text either: the search
        // stopped before the end and absence was never established.
        let inverted = ContentMatcher::compile(&ContentQuery {
            inverted: true,
            ..query("TODO")
        })
        .expect("inverted");
        assert_eq!(
            search_bytes(&inverted, b"head\x00 nothing\n"),
            Outcome::Binary
        );

        // A hex search turns binary detection off, because a byte pattern is
        // precisely a question about a binary file.
        let hex = ContentMatcher::compile(&ContentQuery {
            mode: TextMode::Hex,
            ..query("DE AD BE EF")
        })
        .expect("hex");
        assert!(matches!(
            search_bytes(&hex, &[0x00, 0x01, 0xDE, 0xAD, 0xBE, 0xEF]),
            Outcome::Match(Some(_))
        ));
    }

    #[test]
    fn the_hex_grammar_is_the_viewers() {
        // one parser, one translation.
        let classes = crate::viewer::find::hex_classes("DE AD ?? EF").expect("parses");
        assert_eq!(hex_regex(&classes), r"(?-u)\xDE\xAD(?s:.)\xEF");
        assert_eq!(
            hex_regex(&crate::viewer::find::hex_classes("0xde,0xad").expect("parses")),
            r"(?-u)\xDE\xAD",
            "the separators and prefixes are the viewer's"
        );

        // And the wildcard really is any byte, a line terminator included,
        // which is what puts such a pattern into multi-line mode.
        let m = ContentMatcher::compile(&ContentQuery {
            mode: TextMode::Hex,
            ..query("41 ?? 42")
        })
        .expect("hex");
        assert!(matches!(search_bytes(&m, b"A\nB"), Outcome::Match(Some(_))));
        assert!(matches!(search_bytes(&m, b"AxB"), Outcome::Match(Some(_))));
    }

    #[test]
    fn a_hex_pattern_of_whole_bytes_stays_line_oriented() {
        // No `??` and no `0A`, so nothing it can match crosses a line break
        // and the search streams rather than buffering the file.
        let m = ContentMatcher::compile(&ContentQuery {
            mode: TextMode::Hex,
            ..query("42 43")
        })
        .expect("hex");
        assert!(!m.multi_line);
        assert!(matches!(
            search_bytes(&m, b"aBC\n"),
            Outcome::Match(Some(_))
        ));
    }

    #[test]
    fn an_unparseable_hex_pattern_is_refused_with_its_reason() {
        let err = ContentMatcher::compile(&ContentQuery {
            mode: TextMode::Hex,
            ..query("DE AD BEE")
        })
        .expect_err("odd digits");
        assert!(err.to_string().contains("whole bytes"), "{err}");
    }

    #[test]
    fn an_empty_pattern_is_refused_rather_than_matching_everything() {
        assert!(ContentMatcher::compile(&query("")).is_err());
        assert!(
            ContentMatcher::compile(&ContentQuery {
                charsets: Charsets {
                    utf8: false,
                    utf16: false,
                    latin1: false,
                    cp437: false,
                },
                ..query("TODO")
            })
            .is_err(),
            "and so is a search with no charset to search in"
        );
    }

    #[test]
    fn each_selected_charset_is_tried_and_the_hit_names_which_one() {
        // `café` in windows-1252 is not `café` in UTF-8, so a UTF-8 search of
        // it finds nothing and a Latin-1 search of it finds the line.
        let latin1: Vec<u8> = b"hello\ncaf\xE9 here\n".to_vec();

        let utf8_only = ContentMatcher::compile(&query("café")).expect("plain");
        assert_eq!(utf8_only.charsets(), &[Charset::Utf8]);
        assert_eq!(search_bytes(&utf8_only, &latin1), Outcome::NoMatch);

        let both = ContentMatcher::compile(&ContentQuery {
            charsets: Charsets {
                utf8: true,
                utf16: false,
                latin1: true,
                cp437: false,
            },
            ..query("café")
        })
        .expect("plain");
        // One reader cannot be rewound, so the same bytes are handed over per
        // charset - which is what `search_charset` is for.
        let mut cursor = Cursor::new(latin1.clone());
        assert_eq!(
            both.search_charset(&mut cursor, Charset::Utf8, "x"),
            Outcome::NoMatch
        );
        let mut cursor = Cursor::new(latin1);
        let Outcome::Match(Some(hit)) = both.search_charset(&mut cursor, Charset::Latin1, "x")
        else {
            panic!("expected a windows-1252 hit");
        };
        assert_eq!(hit.charset, "windows-1252");
        assert_eq!(hit.line, Some(2));
        assert_eq!(hit.line_text, "café here");
    }

    #[test]
    fn utf16_is_the_searchers_own_decoding() {
        let mut bytes = Vec::new();
        for unit in "hello\nwidely spaced\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let m = ContentMatcher::compile(&ContentQuery {
            charsets: Charsets {
                utf8: false,
                utf16: true,
                latin1: false,
                cp437: false,
            },
            ..query("spaced")
        })
        .expect("plain");
        let Outcome::Match(Some(hit)) = search_bytes(&m, &bytes) else {
            panic!("expected a UTF-16 hit");
        };
        assert_eq!(hit.charset, "UTF-16");
        assert_eq!(hit.line, Some(2));
    }

    #[test]
    fn cp437_is_transcoded_here_because_encoding_rs_has_no_dos_pages() {
        // 0xDB is a full block in CP437 and is not valid UTF-8 on its own.
        let bytes: Vec<u8> = b"plain\nblock \xDB here\n".to_vec();
        let m = ContentMatcher::compile(&ContentQuery {
            charsets: Charsets {
                utf8: false,
                utf16: false,
                latin1: false,
                cp437: true,
            },
            ..query("block \u{2588}")
        })
        .expect("plain");
        let Outcome::Match(Some(hit)) = search_bytes(&m, &bytes) else {
            panic!("expected a cp437 hit");
        };
        assert_eq!(hit.charset, "cp437");
        assert_eq!(hit.line, Some(2));
    }

    #[test]
    fn the_cp437_reader_streams_and_never_loses_a_byte() {
        // Every byte value, transcoded through a reader that is asked for one
        // byte at a time: the boundary case a chunked transcoder gets wrong.
        let source: Vec<u8> = (0..=255u8).collect();
        let mut reader = Cp437Reader::new(Cursor::new(source.clone()));
        let mut out = Vec::new();
        let mut one = [0u8; 1];
        loop {
            match reader.read(&mut one) {
                Ok(0) => break,
                Ok(_) => out.push(one[0]),
                Err(err) => panic!("{err}"),
            }
        }
        let text = String::from_utf8(out).expect("valid UTF-8 comes out");
        let expected: String = source
            .iter()
            .map(|b| crate::viewer::decode::CP437.table[usize::from(*b)])
            .collect();
        assert_eq!(text, expected);

        // And a zero-length buffer is not an end of file.
        let mut reader = Cp437Reader::new(Cursor::new(vec![b'a']));
        assert_eq!(reader.read(&mut []).expect("empty"), 0);
        let mut buf = [0u8; 8];
        assert_eq!(reader.read(&mut buf).expect("one"), 1);
        assert_eq!(reader.read(&mut buf).expect("eof"), 0);
    }

    #[test]
    fn one_hex_grammar_two_engines() {
        // `DE AD ?? EF` compiled by the viewer's
        // parser and by this module's translation must match the same byte
        // sequences. A sweep rather than an example, because the two engines
        // are genuinely different code and only agreement over a range of
        // inputs is evidence.
        let patterns = [
            "41 42",
            "41 ?? 43",
            "?? 42",
            "0A",
            "41 0A 42",
            "?? ??",
            "FF",
            "41 42 43 44",
        ];
        // A deterministic pseudo-random buffer, plus the interesting literals.
        let mut haystacks: Vec<Vec<u8>> = vec![
            b"ABC".to_vec(),
            b"A\nB".to_vec(),
            b"AB\nCD".to_vec(),
            vec![0x00, 0x41, 0x42, 0xFF],
            vec![0xFF; 4],
            Vec::new(),
        ];
        let mut state: u32 = 0x1234_5678;
        for _ in 0..24 {
            let mut hay = Vec::new();
            for _ in 0..48 {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let byte = u8::try_from((state >> 16) & 0xFF).unwrap_or(0);
                // Biased towards the bytes the patterns name, so the sweep
                // produces matches rather than a wall of "no".
                hay.push(match byte % 5 {
                    0 => b'A',
                    1 => b'B',
                    2 => b'\n',
                    3 => 0xFF,
                    _ => byte,
                });
            }
            haystacks.push(hay);
        }

        for pattern in patterns {
            let classes = crate::viewer::find::hex_classes(pattern).expect("parses");
            let viewer = crate::viewer::find::Matcher::hex(pattern).expect("compiles");
            let ours = ContentMatcher::compile(&ContentQuery {
                mode: TextMode::Hex,
                ..query(pattern)
            })
            .expect("compiles");
            assert!(
                !hex_regex(&classes).is_empty(),
                "the translation is not empty"
            );
            for hay in &haystacks {
                let expected = viewer.find_in(hay, 0).is_some();
                let got = matches!(search_bytes(&ours, hay), Outcome::Match(_));
                assert_eq!(
                    got, expected,
                    "pattern {pattern:?} over {hay:?}: the viewer says {expected}"
                );
            }
        }
    }

    #[test]
    fn a_compiled_query_is_shareable_across_walker_threads() {
        // `ignore`'s parallel walker hands every thread the same `Compiled`,
        // so this is a compile-time proof of the sharing the walk depends on.
        const fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ContentMatcher>();
        assert_send_sync::<crate::search::query::Compiled>();
    }

    #[test]
    fn a_file_that_cannot_be_read_is_skipped_and_says_why() {
        let missing = std::path::Path::new("/nonexistent/holoscommander/search/probe.txt");
        let m = ContentMatcher::compile(&query("TODO")).expect("plain");
        match m.search(missing) {
            Outcome::Skipped(why) => assert!(why.contains("probe.txt"), "{why}"),
            other => panic!("expected a skip, got {other:?}"),
        }
    }

    #[test]
    fn a_hit_says_whether_its_offset_is_the_files_own() {
        // the "opens the viewer at the matching line" is served from
        // the offset for UTF-8 and from the line number for the other three,
        // and only the hit knows which. `grep_searcher` reports positions in
        // the stream it read; for UTF-16, windows-1252 and CP437 that stream is
        // the decoded one, so the number is not a file offset and the viewer
        // must not seek to it.
        let text = "padding one\npadding two\nNEEDLE here\n";
        let m = ContentMatcher::compile(&ContentQuery {
            charsets: Charsets {
                utf8: true,
                utf16: true,
                latin1: true,
                cp437: true,
            },
            ..query("NEEDLE")
        })
        .expect("plain");

        // UTF-8: decoding is the identity, so the offset is the file's and it
        // is the real start of the matching line.
        let mut cursor = Cursor::new(text.as_bytes().to_vec());
        let Outcome::Match(Some(hit)) = m.search_charset(&mut cursor, Charset::Utf8, "x") else {
            panic!("expected a UTF-8 hit");
        };
        assert!(!hit.decoded, "UTF-8 is not transcoded");
        let line_at = text.find("NEEDLE").expect("the needle is in there") as u64;
        assert_eq!(hit.offset, line_at);
        assert_eq!(hit.line, Some(3));

        // UTF-16LE: every character is two bytes, so the searcher's offset is
        // about half of the file's - the exact gap the flag exists to stop the
        // viewer from seeking into.
        let mut utf16 = Vec::new();
        for unit in text.encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        let file_at = line_at.saturating_mul(2);
        let mut cursor = Cursor::new(utf16);
        let Outcome::Match(Some(hit)) = m.search_charset(&mut cursor, Charset::Utf16, "x") else {
            panic!("expected a UTF-16 hit");
        };
        assert!(hit.decoded, "the searcher read the decoded stream");
        assert_ne!(hit.offset, file_at, "which is not where the line starts");
        assert_eq!(hit.line, Some(3), "the line number survives decoding");
        assert!(Charset::Utf16.is_transcoded());
        assert!(Charset::Latin1.is_transcoded());
        assert!(Charset::Cp437.is_transcoded());
        assert!(!Charset::Utf8.is_transcoded());
    }
}
