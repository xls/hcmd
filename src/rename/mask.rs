//! The rename mask language.
//!
//! the design gives the placeholder table and the design
//! fixes the four things the table leaves open. This module is the whole of
//! that language and the only place in the crate where `[N2-5]` means anything:
//!
//! | Written | Means |
//! |---|---|
//! | `[N]` | the whole name, without its extension |
//! | `[N3]` | the 3rd character of it, 1-based |
//! | `[N2-5]` | characters 2 to 5 inclusive |
//! | `[N2-]` | character 2 to the end |
//! | `[E]` | the extension, in either mask |
//! | `[E1-3]`, `[E2]`, `[E2-]` | the same three range forms over the extension |
//! | `[C]` | the counter |
//! | `[Y]` `[M]` `[D]` | mtime year (4 digits), month, day (2 each) |
//! | `[YMD]` | `20260828` |
//! | `[h]` `[m]` `[s]` | mtime hour, minute, second, 2 digits each |
//! | `[hms]` | `143005` |
//! | `[P]` | the parent directory's name |
//!
//! # The four decisions, and why
//!
//! * **Indices are 1-based and count grapheme clusters**.
//!   Bytes would cut a multi-byte character in half and scalar values would
//!   count a combining sequence twice; [`crate::ui::text`] already counts
//!   clusters and a user counts what they can see.
//! * **An out-of-range index yields the empty string.** Never a panic, never a
//!   clamp that quietly returns a different character - so `[N9]` on a
//!   three-character name contributes nothing rather than a surprise.
//! * **"Extension" is [`crate::vfs::Entry::split_name`]'s**: the part after the
//!   *last* dot, which is how the `ext` column, the `Copy <n>` rule and `F2`'s
//!   preselection already spell it. `archive.tar.gz`
//!   has the extension `gz`.
//! * **An unrecognised `[…]` is a literal, brackets included**, so
//!   `[2026] notes.txt` can be typed. The cost is that there is no
//!   escape for a recognised placeholder, which the contract records as a known
//!   gap rather than solving with an escape character the design does not have.
//!
//! Parsing therefore never fails, and expansion never panics: there is no
//! index into a slice in this file that is not a [`slice::get`], and every
//! arithmetic step saturates.
//!
//! [`slice::get`]: slice

use chrono::{DateTime, Datelike, Local, Timelike};
use unicode_segmentation::UnicodeSegmentation;

/// The most characters one expanded name may reach before it is refused.
///
/// `NAME_MAX` on every Linux filesystem worth naming, and the bound
/// [`crate::rename::plan::RenameStatus::TooLong`] is judged against. Bytes,
/// not characters: the kernel counts bytes.
pub const MAX_NAME_BYTES: usize = 255;

/// One piece of a rename mask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// Anything that is not a recognised placeholder, **brackets
    /// included**.
    Literal(String),
    /// `[N]`, `[N3]`, `[N2-5]`, `[N2-]` - a range of the name without its
    /// extension. Indices are 1-based grapheme clusters.
    Name(Range),
    /// `[E]`, `[E1-3]` - the extension, usable in either mask.
    Ext(Range),
    /// `[C]` - the counter.
    Counter,
    /// `[P]` - the **parent directory of the file's real home**, which on a
    /// virtual listing is not the panel's path.
    Parent,
    /// A date or time part of the mtime.
    Date(DatePart),
}

/// `[Y]`, `[M]`, `[D]`, `[YMD]`, `[h]`, `[m]`, `[s]`, `[hms]`.
///
/// Case matters, and deliberately: the design spells the date parts upper
/// and the time parts lower, and `[M]` is a month while `[m]` is a minute.
/// Accepting either case for either would make one of the two unreachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatePart {
    /// `[Y]`, four digits.
    Year,
    /// `[M]`, two digits.
    Month,
    /// `[D]`, two digits.
    Day,
    /// `[YMD]`, eight digits.
    Ymd,
    /// `[h]`, two digits, 24-hour.
    Hour,
    /// `[m]`, two digits.
    Minute,
    /// `[s]`, two digits.
    Second,
    /// `[hms]`, six digits.
    Hms,
}

impl DatePart {
    /// Every part, in the order the table lists them. Drives the
    /// dialog's insert buttons, so the buttons cannot drift from the parser.
    pub const ALL: &'static [Self] = &[
        Self::Year,
        Self::Month,
        Self::Day,
        Self::Ymd,
        Self::Hour,
        Self::Minute,
        Self::Second,
        Self::Hms,
    ];

    /// The placeholder as written, `[Y]` included.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Year => "[Y]",
            Self::Month => "[M]",
            Self::Day => "[D]",
            Self::Ymd => "[YMD]",
            Self::Hour => "[h]",
            Self::Minute => "[m]",
            Self::Second => "[s]",
            Self::Hms => "[hms]",
        }
    }

    /// `2026`, `08`, `20260828`, `143005`.
    ///
    /// Built from `chrono`'s accessors rather than from a format string: a
    /// format string is parsed at run time and `chrono` panics on a bad one,
    /// and this file may not panic.
    pub fn render(self, at: DateTime<Local>) -> String {
        let year = at.year();
        match self {
            // A year before 0 or past 9999 is not a date any filesystem is
            // going to report, but it is representable, so the width is a
            // minimum rather than a promise.
            Self::Year => format!("{year:04}"),
            Self::Month => format!("{:02}", at.month()),
            Self::Day => format!("{:02}", at.day()),
            Self::Ymd => format!("{year:04}{:02}{:02}", at.month(), at.day()),
            Self::Hour => format!("{:02}", at.hour()),
            Self::Minute => format!("{:02}", at.minute()),
            Self::Second => format!("{:02}", at.second()),
            Self::Hms => format!("{:02}{:02}{:02}", at.hour(), at.minute(), at.second()),
        }
    }
}

/// A 1-based, inclusive character range.
///
/// `[N2-5]` is `Range { from: 2, to: Some(5) }`, `[N3]` is
/// `from: 3, to: Some(3)`, `[N2-]` is `from: 2, to: None`, and `[N]` is
/// [`Range::WHOLE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    /// The first character, 1-based.
    pub from: usize,
    /// The last character, 1-based and inclusive. `None` runs to the end.
    pub to: Option<usize>,
}

impl Range {
    /// `[N]`: everything.
    pub const WHOLE: Self = Self { from: 1, to: None };

    /// The slice of `text`, by grapheme cluster.
    ///
    /// An out-of-range index yields the empty string; nothing here can panic or
    /// index out of bounds. `from` of 0 is impossible from the parser - `[N0]`
    /// is not a recognised range - but is handled anyway, because a `Range` is
    /// a public type anybody can build.
    pub fn slice(self, text: &str) -> String {
        if self.from == 0 {
            return String::new();
        }
        let skip = self.from.saturating_sub(1);
        let take = match self.to {
            // `to` before `from` is an empty range, not a reversed one.
            Some(to) => to.saturating_add(1).saturating_sub(self.from),
            None => usize::MAX,
        };
        if take == 0 {
            return String::new();
        }
        text.graphemes(true).skip(skip).take(take).collect()
    }
}

/// A parsed mask.
///
/// Parsing never fails: an unrecognised `[…]` becomes a [`Token::Literal`]
/// with its brackets intact.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Mask {
    tokens: Vec<Token>,
}

impl Mask {
    /// The name mask's default, `[N]`.
    pub fn default_name() -> Self {
        Self {
            tokens: vec![Token::Name(Range::WHOLE)],
        }
    }

    /// The extension mask's default, `[E]`.
    pub fn default_ext() -> Self {
        Self {
            tokens: vec![Token::Ext(Range::WHOLE)],
        }
    }

    /// Parse one mask. Never fails; see the module docs.
    pub fn parse(text: &str) -> Self {
        let mut tokens: Vec<Token> = Vec::new();
        let mut literal = String::new();
        let mut rest = text;
        while let Some(open) = rest.find('[') {
            let (head, tail) = rest.split_at(open);
            literal.push_str(head);
            // `tail` starts at the `[`. Everything up to the first `]` is the
            // candidate; a `[` with no `]` after it is literal to the end.
            let Some(close) = tail.find(']') else {
                literal.push_str(tail);
                rest = "";
                break;
            };
            let inner = tail.get(1..close).unwrap_or("");
            match placeholder(inner) {
                Some(token) => {
                    if !literal.is_empty() {
                        tokens.push(Token::Literal(std::mem::take(&mut literal)));
                    }
                    tokens.push(token);
                }
                // Not a placeholder: the brackets are part of the name.
                None => literal.push_str(tail.get(..=close).unwrap_or("")),
            }
            rest = tail.get(close.saturating_add(1)..).unwrap_or("");
        }
        literal.push_str(rest);
        if !literal.is_empty() {
            tokens.push(Token::Literal(literal));
        }
        Self { tokens }
    }

    /// The tokens, in order.
    pub fn tokens(&self) -> &[Token] {
        &self.tokens
    }

    /// True when nothing in it reads the mtime.
    ///
    /// A row with no date is only flagged
    /// [`crate::rename::plan::RenameStatus::NoDate`] for a mask that
    /// asked.
    pub fn uses_date(&self) -> bool {
        self.tokens.iter().any(|t| matches!(t, Token::Date(_)))
    }

    /// True when it contains `[C]`.
    pub fn uses_counter(&self) -> bool {
        self.tokens.iter().any(|t| matches!(t, Token::Counter))
    }

    /// Expand against one row.
    pub fn expand(&self, ctx: &Context<'_>) -> String {
        let mut out = String::new();
        for token in &self.tokens {
            match token {
                Token::Literal(text) => out.push_str(text),
                Token::Name(range) => out.push_str(&range.slice(ctx.stem)),
                Token::Ext(range) => out.push_str(&range.slice(ctx.ext)),
                Token::Counter => out.push_str(ctx.counter),
                Token::Parent => out.push_str(ctx.parent),
                // A row with no mtime expands a date placeholder to nothing
                // and is flagged `NoDate`, which warns without refusing.
                Token::Date(part) => {
                    if let Some(at) = ctx.mtime {
                        out.push_str(&part.render(at));
                    }
                }
            }
        }
        out
    }
}

/// The placeholder `inner` names, or `None` when it is not one.
///
/// `inner` is what stood between the brackets, so `[N2-5]` arrives as `N2-5`.
fn placeholder(inner: &str) -> Option<Token> {
    match inner {
        "C" => return Some(Token::Counter),
        "P" => return Some(Token::Parent),
        "Y" => return Some(Token::Date(DatePart::Year)),
        "M" => return Some(Token::Date(DatePart::Month)),
        "D" => return Some(Token::Date(DatePart::Day)),
        "YMD" => return Some(Token::Date(DatePart::Ymd)),
        "h" => return Some(Token::Date(DatePart::Hour)),
        "m" => return Some(Token::Date(DatePart::Minute)),
        "s" => return Some(Token::Date(DatePart::Second)),
        "hms" => return Some(Token::Date(DatePart::Hms)),
        _ => {}
    }
    if let Some(rest) = inner.strip_prefix('N') {
        return range(rest).map(Token::Name);
    }
    if let Some(rest) = inner.strip_prefix('E') {
        return range(rest).map(Token::Ext);
    }
    None
}

/// The range half of `[N2-5]`, or `None` when it is not one.
///
/// The empty string is [`Range::WHOLE`], which is what makes `[N]` and `[E]`
/// the same code path as `[N2-5]`. A `0` bound is refused rather than clamped:
/// there is no character zero, and accepting it would make `[N0-2]` mean
/// something a user cannot predict.
fn range(text: &str) -> Option<Range> {
    if text.is_empty() {
        return Some(Range::WHOLE);
    }
    let index = |s: &str| -> Option<usize> {
        let n: usize = s.parse().ok()?;
        (n > 0).then_some(n)
    };
    match text.split_once('-') {
        Some((from, "")) => index(from).map(|from| Range { from, to: None }),
        Some((from, to)) => {
            let from = index(from)?;
            let to = index(to)?;
            Some(Range { from, to: Some(to) })
        }
        None => index(text).map(|n| Range {
            from: n,
            to: Some(n),
        }),
    }
}

/// Everything a mask can read about one row.
#[derive(Debug, Clone)]
pub struct Context<'a> {
    /// The name without its extension, by [`crate::vfs::Entry::split_name`]'s
    /// definition - the part before the **last** dot - so the whole program
    /// spells "extension" one way.
    pub stem: &'a str,
    /// The extension, without its dot.
    pub ext: &'a str,
    /// The real home's parent directory name, or `""`.
    pub parent: &'a str,
    /// The mtime in local time, `None` when the row has none.
    pub mtime: Option<DateTime<Local>>,
    /// This row's counter value, already stepped and zero-padded.
    pub counter: &'a str,
}

/// The `[C]` definition (the "Define counter").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counter {
    /// `Start at`.
    pub start: i64,
    /// `Step by`.
    pub step: i64,
    /// `Digits`: the zero-padding width.
    pub digits: u8,
}

impl Counter {
    /// `start = 1`, `step = 1`, `digits = 1`, which is Total Commander's.
    pub const DEFAULT: Self = Self {
        start: 1,
        step: 1,
        digits: 1,
    };

    /// The value for row `index` (0-based) of the table **in its current sort
    /// order**, which is what the design means by "the sort determines
    /// counter order".
    ///
    /// Arithmetic saturates rather than wrapping: a step of `i64::MAX` is a
    /// typo, and a typo should produce a silly name rather than a different
    /// silly name. A negative value renders with its sign in front of the
    /// padding, so `-5` at three digits is `-005` and the digits the user asked
    /// for are the digits they get.
    pub fn value(&self, index: usize) -> String {
        let index = i64::try_from(index).unwrap_or(i64::MAX);
        let value = self.start.saturating_add(self.step.saturating_mul(index));
        let width = usize::from(self.digits);
        let body = format!("{:0width$}", value.unsigned_abs());
        if value < 0 { format!("-{body}") } else { body }
    }
}

impl Default for Counter {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// 2026-08-28 14:30:05 local, which is the contract's own example.
    fn at() -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 8, 28, 14, 30, 5)
            .single()
            .expect("an unambiguous local time")
    }

    fn ctx<'a>(stem: &'a str, ext: &'a str, parent: &'a str, counter: &'a str) -> Context<'a> {
        Context {
            stem,
            ext,
            parent,
            mtime: Some(at()),
            counter,
        }
    }

    fn expand(mask: &str, stem: &str, ext: &str) -> String {
        Mask::parse(mask).expand(&ctx(stem, ext, "photos", "7"))
    }

    #[test]
    fn every_placeholder_in_the_table_expands_to_what_the_table_says() {
        // the table, row by row. This test is the table.
        assert_eq!(expand("[N]", "report", "txt"), "report");
        assert_eq!(expand("[N3]", "report", "txt"), "p");
        assert_eq!(expand("[N2-5]", "report", "txt"), "epor");
        assert_eq!(expand("[N2-]", "report", "txt"), "eport");
        assert_eq!(expand("[E]", "report", "txt"), "txt");
        assert_eq!(expand("[E1-3]", "report", "jpeg"), "jpe");
        assert_eq!(expand("[E2]", "report", "jpeg"), "p");
        assert_eq!(expand("[E2-]", "report", "jpeg"), "peg");
        assert_eq!(expand("[C]", "report", "txt"), "7");
        assert_eq!(expand("[Y]", "report", "txt"), "2026");
        assert_eq!(expand("[M]", "report", "txt"), "08");
        assert_eq!(expand("[D]", "report", "txt"), "28");
        assert_eq!(expand("[YMD]", "report", "txt"), "20260828");
        assert_eq!(expand("[h]", "report", "txt"), "14");
        assert_eq!(expand("[m]", "report", "txt"), "30");
        assert_eq!(expand("[s]", "report", "txt"), "05");
        assert_eq!(expand("[hms]", "report", "txt"), "143005");
        assert_eq!(expand("[P]", "report", "txt"), "photos");
    }

    #[test]
    fn placeholders_compose_with_literals_around_them() {
        assert_eq!(expand("[N]_[C]", "a", "txt"), "a_7");
        assert_eq!(expand("[YMD]-[N1-3]", "report", "txt"), "20260828-rep");
        assert_eq!(expand("scan", "report", "txt"), "scan");
        assert_eq!(expand("", "report", "txt"), "");
    }

    #[test]
    fn an_unrecognised_placeholder_is_a_literal_with_its_brackets() {
        // `[2026] report.txt` has to be renameable, so an unknown `[…]`
        // survives intact rather than vanishing.
        assert_eq!(expand("[2026] [N]", "notes", "txt"), "[2026] notes");
        assert_eq!(expand("[n]", "notes", "txt"), "[n]", "case is exact");
        assert_eq!(expand("[NX]", "notes", "txt"), "[NX]");
        assert_eq!(expand("[N0]", "notes", "txt"), "[N0]", "no character zero");
        assert_eq!(expand("[N2-1]", "notes", "txt"), "", "an empty range");
        assert_eq!(expand("[", "notes", "txt"), "[", "an unclosed bracket");
        assert_eq!(expand("a]b", "notes", "txt"), "a]b");
        assert_eq!(expand("[]", "notes", "txt"), "[]");
    }

    #[test]
    fn an_out_of_range_index_yields_nothing_rather_than_a_panic() {
        assert_eq!(expand("[N9]", "abc", "txt"), "");
        assert_eq!(expand("[N2-99]", "abc", "txt"), "bc");
        assert_eq!(expand("[N9-]", "abc", "txt"), "");
        assert_eq!(expand("[E4]", "abc", "txt"), "");
        assert_eq!(expand("[N1-3]", "", ""), "");
    }

    #[test]
    fn indices_count_grapheme_clusters_and_not_bytes() {
        // A byte index would slice these in half.
        assert_eq!(expand("[N2]", "héllo", ""), "é");
        assert_eq!(expand("[N1-2]", "日本語", ""), "日本");
        // A combining sequence is one character to the user and must be one
        // here: `e` + U+0301 is one cluster.
        assert_eq!(expand("[N1]", "e\u{301}x", ""), "e\u{301}");
        assert_eq!(expand("[N2]", "e\u{301}x", ""), "x");
        // An emoji with a zero-width joiner is still one.
        assert_eq!(
            expand("[N1]", "\u{1f469}\u{200d}\u{1f4bb}a", ""),
            "\u{1f469}\u{200d}\u{1f4bb}"
        );
    }

    #[test]
    fn a_date_placeholder_on_a_row_with_no_mtime_expands_to_nothing() {
        // a warning, not a refusal. `plan` flags it `NoDate`.
        let mask = Mask::parse("[N]-[YMD]");
        let out = mask.expand(&Context {
            stem: "a",
            ext: "txt",
            parent: "",
            mtime: None,
            counter: "1",
        });
        assert_eq!(out, "a-");
        assert!(mask.uses_date());
        assert!(!Mask::parse("[N]").uses_date());
        assert!(Mask::parse("[N][C]").uses_counter());
        assert!(!Mask::parse("[N]").uses_counter());
    }

    #[test]
    fn the_defaults_are_the_ones_spec_15_1_names() {
        assert_eq!(Mask::default_name(), Mask::parse("[N]"));
        assert_eq!(Mask::default_ext(), Mask::parse("[E]"));
        assert_eq!(
            Mask::parse("[N]").tokens(),
            &[Token::Name(Range::WHOLE)][..]
        );
    }

    #[test]
    fn every_date_tag_the_dialog_offers_parses_back_to_its_own_part() {
        // The insert buttons are built from `DatePart::ALL` and the parser is
        // a separate `match`; this is what stops the two drifting apart.
        for part in DatePart::ALL {
            let mask = Mask::parse(part.tag());
            assert_eq!(
                mask.tokens(),
                &[Token::Date(*part)][..],
                "{} did not parse back",
                part.tag()
            );
        }
    }

    #[test]
    fn the_counter_steps_pads_and_saturates() {
        // start 10, step 5, 3 digits.
        let c = Counter {
            start: 10,
            step: 5,
            digits: 3,
        };
        let values: Vec<String> = (0..4).map(|i| c.value(i)).collect();
        assert_eq!(values, vec!["010", "015", "020", "025"]);

        assert_eq!(Counter::DEFAULT.value(0), "1");
        assert_eq!(Counter::DEFAULT.value(41), "42");
        assert_eq!(Counter::default(), Counter::DEFAULT);

        // A negative value keeps its sign in front of the padding.
        let down = Counter {
            start: 1,
            step: -3,
            digits: 3,
        };
        assert_eq!(down.value(2), "-005");

        // And nothing overflows.
        let silly = Counter {
            start: i64::MAX,
            step: i64::MAX,
            digits: 1,
        };
        assert_eq!(silly.value(9), i64::MAX.to_string());
    }

    #[test]
    fn no_rename_mask_can_panic() {
        // Contract, the fuzz-shaped half: 4096 generated masks against
        // generated names. Nothing here asserts on the *output* - the point is
        // that expansion always produces one.
        const PIECES: [&str; 16] = [
            "[N",
            "]",
            "[",
            "N2-5",
            "[E1-",
            "[C]",
            "[YMD]",
            "-",
            "[P]",
            "[m]",
            "0",
            "99999999999999999999",
            "]]",
            "[[",
            "\u{1f600}",
            "é",
        ];
        const NAMES: [&str; 6] = [
            "",
            "a",
            "e\u{301}x",
            "日本語",
            "a.b.c",
            "\u{1f469}\u{200d}\u{1f4bb}",
        ];
        let mut seed = 0x2026_0828_u64;
        for _ in 0..4096 {
            let mut mask = String::new();
            for _ in 0..6 {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                let pick = usize::try_from(seed >> 33).unwrap_or(0) % PIECES.len();
                mask.push_str(PIECES.get(pick).copied().unwrap_or(""));
            }
            let parsed = Mask::parse(&mask);
            for name in NAMES {
                for ext in NAMES {
                    let out = parsed.expand(&ctx(name, ext, "p", "1"));
                    // Reaching here at all is most of the test; determinism is
                    // the rest, because a mask that expanded differently on
                    // the second keystroke would make the preview a lie.
                    assert_eq!(out, parsed.expand(&ctx(name, ext, "p", "1")));
                }
            }
        }
    }
}
