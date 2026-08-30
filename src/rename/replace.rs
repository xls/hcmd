//! Search and replace, and the case dropdown.
//!
//! the second and third control groups:
//!
//! > **Search & Replace** - `Search for` and `Replace with` combos, with
//! > toggles: `1x`, `[E]`, `RegEx`, `Subst.`, case toggle.
//! >
//! > **Upper/lowercase** - a dropdown: `Unchanged`, `lower`, `UPPER`,
//! > `First capital`, `Each Word Capital`.
//!
//! # The two decisions this file had to make
//!
//! * **The `^` toggle is case sensitivity, default off**.
//!   the design flags it as unconfirmed and the design lists it as undecided. Every
//!   other case control in this program - quick search, the mark mask, the
//!   content search - is a case-sensitivity toggle, and one meaning for one
//!   idea beats a guess at a foreign UI.
//! * **`Each Word Capital` breaks words on whitespace, `_`, `-` and `.`**.
//!   Those four are what separate words in filenames: a
//!   wider set would capitalise after every digit and a narrower one would
//!   leave `my_file` as `My_file`.
//!
//! # Two engines, deliberately
//!
//! A literal search is matched here, character by character, and a regular
//! expression goes to `grep-regex` - the crate's one regex engine.
//! The literal half is not routed through the regex
//! engine because a literal search must never fail to compile: a user typing
//! `report (1)` into `Search for` with `RegEx` off is typing a filename, and
//! an unbalanced parenthesis in a filename is not an error.

use grep_matcher::{Captures, Matcher};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};

use crate::error::{Error, Result};

/// the five toggles, and the two fields they qualify.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Replace {
    /// `Search for`.
    pub search: String,
    /// `Replace with`.
    pub with: String,
    /// `1x`: the first occurrence only.
    pub first_only: bool,
    /// `[E]`: apply to the extension as well as the name.
    pub include_ext: bool,
    /// `RegEx`: the search is a regular expression.
    pub regex: bool,
    /// `Subst.`: `$1` backreferences in the replacement. Only meaningful with
    /// `regex`; with it off, a `$1` in the replacement is literal.
    pub substitute: bool,
    /// the `^` control, whose semantics the design leaves to be decided.
    /// **Decided here as case sensitivity**, default off.
    pub match_case: bool,
}

impl Replace {
    /// Nothing to do.
    ///
    /// An empty `Search for` is the whole test: replacing nothing with
    /// something would insert it at every position, which is never what an
    /// empty field means.
    pub fn is_empty(&self) -> bool {
        self.search.is_empty()
    }

    /// Compile it. The one place a bad regex is reported.
    pub fn compile(&self) -> Result<CompiledReplace> {
        let engine = if self.regex {
            // Unanchored, and case-insensitive unless the `^` toggle says
            // otherwise - the same two rules `ops::mask` and the content
            // search follow.
            let matcher = RegexMatcherBuilder::new()
                .case_insensitive(!self.match_case)
                .build(&self.search)
                .map_err(|e| Error::msg(format!("search pattern: {e}")))?;
            Engine::Regex(Box::new(matcher))
        } else {
            Engine::Literal(self.search.clone())
        };
        Ok(CompiledReplace {
            engine,
            with: self.with.clone(),
            first_only: self.first_only,
            // A `$1` with `RegEx` off has no capture groups to name, so it is
            // literal text; the design ties `Subst.` to backreferences and
            // backreferences need a regex.
            substitute: self.substitute && self.regex,
            match_case: self.match_case,
        })
    }
}

/// Which engine a [`CompiledReplace`] matches with.
#[derive(Debug)]
enum Engine {
    /// A plain string, matched here.
    Literal(String),
    /// A regular expression, matched by `grep-regex`.
    Regex(Box<RegexMatcher>),
}

/// A [`Replace`] with its pattern compiled, ready to run over every row.
///
/// Compiled once per preview rather than once per row: the design rebuilds
/// the New name column on every keystroke, and compiling a regex ten thousand
/// times a keystroke is the difference between a preview and a stall.
#[derive(Debug)]
pub struct CompiledReplace {
    engine: Engine,
    with: String,
    first_only: bool,
    substitute: bool,
    match_case: bool,
}

impl CompiledReplace {
    /// Apply it to one piece of a name.
    ///
    /// The caller decides *which* pieces: the pipeline runs this over the stem
    /// always and over the extension only when [`Replace::include_ext`] is
    /// set.
    pub fn apply(&self, text: &str) -> String {
        match &self.engine {
            Engine::Literal(needle) => self.apply_literal(needle, text),
            Engine::Regex(matcher) => self.apply_regex(matcher, text),
        }
    }

    /// A literal search, matched character by character.
    ///
    /// Case folding is per character (`a.eq_ignore_ascii_case(b)` widened to
    /// Unicode through `char::to_lowercase`), which means the one-to-many
    /// foldings - `ß` against `SS` - do not match. That is a boring rule a user
    /// can predict, and the alternative is a lowercased copy whose byte offsets
    /// no longer line up with the original.
    fn apply_literal(&self, needle: &str, text: &str) -> String {
        if needle.is_empty() {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some((start, end)) = self.find_literal(needle, rest) {
            out.push_str(rest.get(..start).unwrap_or(""));
            out.push_str(&self.with);
            rest = rest.get(end..).unwrap_or("");
            if self.first_only {
                break;
            }
        }
        out.push_str(rest);
        out
    }

    /// The first occurrence of `needle` in `hay`, as a byte range.
    fn find_literal(&self, needle: &str, hay: &str) -> Option<(usize, usize)> {
        if self.match_case {
            let start = hay.find(needle)?;
            return Some((start, start.saturating_add(needle.len())));
        }
        // Case-insensitively there is no `find`, so this walks the character
        // boundaries and tries each one. Filenames are short and this runs
        // once per row per keystroke.
        for (start, _) in hay.char_indices() {
            let tail = hay.get(start..).unwrap_or("");
            let mut chars = tail.chars();
            let mut consumed = 0usize;
            let mut ok = true;
            for want in needle.chars() {
                match chars.next() {
                    Some(have) if have.to_lowercase().eq(want.to_lowercase()) => {
                        consumed = consumed.saturating_add(have.len_utf8());
                    }
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                return Some((start, start.saturating_add(consumed)));
            }
        }
        None
    }

    /// A regular expression, matched by `grep-regex` (the engine).
    ///
    /// `grep-matcher` works in bytes and reports byte offsets into the
    /// haystack it was given, so every slice here goes through `get` and the
    /// result is decoded lossily: a `(?-u)` pattern is allowed to cut a
    /// character in half, and a replacement string is not a place to panic.
    fn apply_regex(&self, matcher: &RegexMatcher, text: &str) -> String {
        let hay = text.as_bytes();
        let Ok(mut caps) = matcher.new_captures() else {
            return text.to_string();
        };
        let mut out: Vec<u8> = Vec::with_capacity(hay.len());
        let mut at = 0usize;
        while at <= hay.len() {
            match matcher.captures_at(hay, at, &mut caps) {
                Ok(true) => {}
                // A matcher that errors mid-string has already contributed
                // whatever it matched; the rest is copied through unchanged,
                // which is the same shape as "no further match".
                Ok(false) | Err(_) => break,
            }
            let Some(whole) = caps.get(0) else {
                break;
            };
            out.extend_from_slice(hay.get(at..whole.start()).unwrap_or(&[]));
            if self.substitute {
                caps.interpolate(
                    |name| matcher.capture_index(name),
                    hay,
                    self.with.as_bytes(),
                    &mut out,
                );
            } else {
                out.extend_from_slice(self.with.as_bytes());
            }
            // An empty match would otherwise spin here for ever: step past one
            // byte, copying it, exactly as every regex replacement does.
            if whole.end() == whole.start() {
                let next = whole.end().saturating_add(1);
                out.extend_from_slice(hay.get(whole.end()..next).unwrap_or(&[]));
                at = next;
            } else {
                at = whole.end();
            }
            if self.first_only {
                break;
            }
        }
        out.extend_from_slice(hay.get(at..).unwrap_or(&[]));
        String::from_utf8_lossy(&out).into_owned()
    }
}

/// the Upper/lowercase dropdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Case {
    /// Leave the name alone. The default.
    #[default]
    Unchanged,
    /// `lower`.
    Lower,
    /// `UPPER`.
    Upper,
    /// First character upper, the rest lower.
    FirstCapital,
    /// First character of each word upper, the rest lower. Words break on
    /// whitespace, `_`, `-` and `.`.
    EachWordCapital,
}

impl Case {
    /// Every choice, in the order, which is the order the dropdown
    /// steps through.
    pub const CHOICES: &'static [Self] = &[
        Self::Unchanged,
        Self::Lower,
        Self::Upper,
        Self::FirstCapital,
        Self::EachWordCapital,
    ];

    /// The dropdown's label, spelled the way the design spells it - the
    /// spelling is the demonstration.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unchanged => "Unchanged",
            Self::Lower => "lower",
            Self::Upper => "UPPER",
            Self::FirstCapital => "First capital",
            Self::EachWordCapital => "Each Word Capital",
        }
    }

    /// A stable string id, for `renames.toml` and for tests.
    ///
    /// Separate from [`Case::label`] on purpose: the label is what the
    /// dropdown shows and is allowed to change with the wording, and the id is
    /// what a saved preset holds and is not.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::Lower => "lower",
            Self::Upper => "upper",
            Self::FirstCapital => "first_capital",
            Self::EachWordCapital => "each_word_capital",
        }
    }

    /// Read one back, or `None` for anything that is not one.
    pub fn from_id(id: &str) -> Option<Self> {
        Self::CHOICES.iter().copied().find(|c| c.id() == id)
    }

    /// Apply it.
    ///
    /// Run over the stem **and** the extension, so `UPPER` produces
    /// `README.TXT` rather than `README.txt`.
    pub fn apply(self, text: &str) -> String {
        match self {
            Self::Unchanged => text.to_string(),
            Self::Lower => text.to_lowercase(),
            Self::Upper => text.to_uppercase(),
            Self::FirstCapital => capitalise(text, |_| false),
            Self::EachWordCapital => capitalise(text, is_word_break),
        }
    }
}

/// Is `c` one of the four characters that separate words in a filename?
fn is_word_break(c: char) -> bool {
    c == '_' || c == '-' || c == '.' || c.is_whitespace()
}

/// Lowercase `text`, upper-casing the first character of each word.
///
/// `at_break` says which characters start a new word; for
/// [`Case::FirstCapital`] nothing does, so only the very first character is
/// raised.
fn capitalise(text: &str, at_break: impl Fn(char) -> bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut start_of_word = true;
    for c in text.chars() {
        if start_of_word {
            out.extend(c.to_uppercase());
        } else {
            out.extend(c.to_lowercase());
        }
        start_of_word = at_break(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replace(search: &str, with: &str) -> Replace {
        Replace {
            search: search.to_string(),
            with: with.to_string(),
            ..Replace::default()
        }
    }

    fn apply(r: &Replace, text: &str) -> String {
        r.compile().expect("compiles").apply(text)
    }

    #[test]
    fn a_literal_replacement_replaces_every_occurrence_and_1x_replaces_one() {
        // the `1x` toggle.
        let all = replace("a", "X");
        assert_eq!(apply(&all, "banana"), "bXnXnX");
        let once = Replace {
            first_only: true,
            ..all
        };
        assert_eq!(apply(&once, "banana"), "bXnana");
    }

    #[test]
    fn a_literal_search_is_case_insensitive_until_the_case_toggle_is_set() {
        // the `^` control is case sensitivity, default off.
        let loose = replace("REPORT", "note");
        assert_eq!(apply(&loose, "Report-report"), "note-note");
        let strict = Replace {
            match_case: true,
            ..loose
        };
        assert_eq!(apply(&strict, "Report-REPORT"), "Report-note");
    }

    #[test]
    fn a_literal_search_is_literal_however_regex_like_it_looks() {
        // The reason the literal half is not routed through the regex engine:
        // a filename is not a pattern and must never fail to compile.
        let r = replace("(1)", "");
        assert_eq!(apply(&r, "report (1).txt"), "report .txt");
        let r = replace(".", "_");
        assert_eq!(apply(&r, "a.b.c"), "a_b_c");
        let r = replace("[N]", "x");
        assert_eq!(apply(&r, "a[N]b"), "axb");
        // And an unbalanced bracket compiles, because it is not compiled.
        assert!(replace("([", "x").compile().is_ok());
    }

    #[test]
    fn a_regex_search_matches_unanchored_and_reports_a_bad_pattern() {
        let r = Replace {
            regex: true,
            ..replace("\\d+", "#")
        };
        assert_eq!(apply(&r, "img2026x07"), "img#x#");

        // unanchored, so an anchor has to be written.
        let anchored = Replace {
            regex: true,
            ..replace("^a", "X")
        };
        assert_eq!(apply(&anchored, "banana"), "banana");
        assert_eq!(apply(&anchored, "apple"), "Xpple");

        // The one place a bad regex is reported.
        let bad = Replace {
            regex: true,
            ..replace("(unclosed", "x")
        };
        let err = bad.compile().expect_err("an unclosed group is refused");
        assert!(err.to_string().contains("search pattern"), "{err}");
    }

    #[test]
    fn subst_interpolates_backreferences_only_when_the_search_is_a_regex() {
        // `Subst.` enables `$1`-style backreferences.
        let on = Replace {
            regex: true,
            substitute: true,
            ..replace("(\\w+)-(\\w+)", "$2-$1")
        };
        assert_eq!(apply(&on, "left-right"), "right-left");

        // With `Subst.` off the replacement is literal text.
        let off = Replace {
            regex: true,
            substitute: false,
            ..replace("(\\w+)-(\\w+)", "$2-$1")
        };
        assert_eq!(apply(&off, "left-right"), "$2-$1");

        // And with `RegEx` off there are no groups to name, so `$1` is text
        // whatever `Subst.` says.
        let no_regex = Replace {
            regex: false,
            substitute: true,
            ..replace("x", "$1")
        };
        assert_eq!(apply(&no_regex, "axb"), "a$1b");
    }

    #[test]
    fn a_regex_that_can_match_nothing_terminates() {
        // The classic empty-match spin. `a*` matches the empty string at every
        // position, and the loop has to step past it.
        let r = Replace {
            regex: true,
            ..replace("a*", "-")
        };
        assert_eq!(apply(&r, "bcd"), "-b-c-d-");
        let r = Replace {
            regex: true,
            first_only: true,
            ..replace("a*", "-")
        };
        assert_eq!(apply(&r, "bcd"), "-bcd");
    }

    #[test]
    fn an_empty_search_changes_nothing() {
        let r = replace("", "X");
        assert!(r.is_empty());
        assert_eq!(apply(&r, "report.txt"), "report.txt");
    }

    #[test]
    fn multibyte_text_survives_both_engines() {
        let r = replace("é", "e");
        assert_eq!(apply(&r, "café"), "cafe");
        let r = Replace {
            regex: true,
            ..replace("[日本]+", "JP")
        };
        assert_eq!(apply(&r, "報告書日本語"), "報告書JP語");
        // A case-insensitive literal over characters whose lowercase is longer
        // must not slice a character in half.
        let r = replace("İ", "i");
        assert_eq!(apply(&r, "İstanbul"), "istanbul");
    }

    #[test]
    fn the_case_dropdown_is_spec_15_1s_five_choices() {
        assert_eq!(Case::CHOICES.len(), 5);
        assert_eq!(Case::default(), Case::Unchanged);
        assert_eq!(Case::Unchanged.apply("My File.TXT"), "My File.TXT");
        assert_eq!(Case::Lower.apply("My File.TXT"), "my file.txt");
        assert_eq!(Case::Upper.apply("My File.txt"), "MY FILE.TXT");
        assert_eq!(Case::FirstCapital.apply("my FILE name"), "My file name");
        assert_eq!(
            Case::EachWordCapital.apply("my_file-name.two words"),
            "My_File-Name.Two Words"
        );
        // those four separators and no others.
        assert_eq!(Case::EachWordCapital.apply("a1b c"), "A1b C");
        for case in Case::CHOICES {
            assert!(!case.label().is_empty());
            assert_eq!(case.apply(""), "");
            assert_eq!(Case::from_id(case.id()), Some(*case), "{}", case.id());
        }
        assert_eq!(Case::from_id("sideways"), None);
    }
}
