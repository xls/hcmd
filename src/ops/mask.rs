//! Wildcard masks.
//!
//! Two features share one matcher, so it is written once:
//!
//! * `+` and `-` - "mark by wildcard" / "unmark by wildcard".
//! * The copy/move dialog's **"Only files of this type"**, "a second mask that
//!   filters what actually gets copied out of the selection".
//!
//! # Syntax
//!
//! Total Commander's, which is what a user typing into either prompt expects:
//!
//! | Form | Meaning |
//! |---|---|
//! | `*` | any run of characters, including none |
//! | `?` | exactly one character |
//! | `*.*` | **everything**, including names with no dot - TC's convention, and the default the copy dialog pre-fills |
//! | `a.txt b.doc` | a list: a name matching any one of them matches |
//! | `a.txt;b.doc` | the same list, semicolon-separated |
//! | `|*.bak` | everything before the `\|`, excluding anything after it |
//!
//! Matching is **case-insensitive**. A mask is typed in a hurry against a
//! listing the user is looking at, and `*.JPG` failing to catch `photo.jpg` is
//! never what was meant.
//!
//! # Regex, since v0.6
//!
//! the design says the `+`/`-` prompt "can be switched to regex". That needed
//! a regex engine, and v0.2 had none: `regex` is not on the dependency table,
//! so [`MaskMode::Regex`] reported itself as deferred rather than silently
//! matching something else.
//!
//! the search brings `grep-regex`, which *is* on that table, and it
//! is the regex engine this mode was waiting for. [`compile`] is the one place
//! a mask in either language is turned into something that matches, so the
//! `+`/`-` prompt, the copy dialog's second mask and the `RegEx`
//! checkbox cannot disagree about what a pattern means.
//!
//! Two rules the regex half inherits deliberately:
//!
//! * **Unanchored**, so `\.rs$` works and `rs` catches everything containing
//!   it. That is ripgrep's default and Total Commander's; an anchored regex is
//!   one `^…$` away and an unanchorable one is not.
//! * **Case-insensitive**, because [`matches`] has been since v0.2 for a stated
//!   reason, and switching the language must not silently switch the case rule.
//!   A user who wants sensitivity writes `(?-i)`.
//!
//! # Why a `Searcher` matches one name
//!
//! `grep_regex::RegexMatcher` answers `is_match` through the `grep-matcher`
//! trait, and `grep-matcher` is not on the table - only `ignore`,
//! `grep-searcher` and `grep-regex` are. So the match is asked of
//! [`grep_searcher::Searcher`], which takes the matcher generically and needs
//! no trait imported. It costs about a hundred nanoseconds per name, which is
//! far below the `stat` that produced the name in the first place.

use crate::error::{Error, Result};

/// Which language a mask is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MaskMode {
    /// `*`, `?`, a list, and a `|` exclusion. The default.
    #[default]
    Wildcard,
    /// A regular expression, unanchored and case-insensitive. See the module
    /// docs.
    Regex,
}

impl MaskMode {
    /// A stable string id.
    pub const fn id(&self) -> &'static str {
        match self {
            Self::Wildcard => "wildcard",
            Self::Regex => "regex",
        }
    }

    /// `None` when the mode works, `Some(reason)` when it does not - which the
    /// prompt shows instead of accepting a mask it cannot honour.
    ///
    /// `None` for both modes since v0.6: `grep-regex` is on the dependency
    /// table as of the search, which is what [`Self::Regex`] was
    /// waiting for. The method stays because the prompt asks the question and
    /// a future mode may have to answer it differently.
    pub const fn unavailable(&self) -> Option<&'static str> {
        match self {
            Self::Wildcard | Self::Regex => None,
        }
    }
}

/// A mask compiled once, so a listing is not recompiled per row.
///
/// The wildcard arm keeps the mask as typed because [`matches`] parses its
/// lists and its `|` clause on the fly and is already linear; the regex arm
/// keeps the built matcher, which is the expensive half.
#[derive(Debug)]
pub enum Compiled {
    /// The mask as typed, matched by [`matches`].
    Wildcard(String),
    /// A built regular expression, matched by [`matches_regex`]. Boxed because
    /// a `RegexMatcher` is far larger than a `String` and this enum is passed
    /// by value.
    Regex(Box<grep_regex::RegexMatcher>),
}

impl Compiled {
    /// Does `name` match?
    pub fn matches(&self, name: &str) -> bool {
        match self {
            Self::Wildcard(mask) => matches(mask, name),
            Self::Regex(matcher) => matches_regex(matcher, name),
        }
    }
}

/// Compile a mask in either language.
///
/// The one place a bad regex is refused, so the mark prompt and the Find
/// dialog report it in the same words.
pub fn compile(mask: &str, mode: MaskMode) -> Result<Compiled> {
    match mode {
        MaskMode::Wildcard => Ok(Compiled::Wildcard(mask.to_string())),
        MaskMode::Regex => grep_regex::RegexMatcherBuilder::new()
            // Case-insensitive by default and overridable with an inline
            // `(?-i)`, which is what keeps the two mask languages agreeing.
            .case_insensitive(true)
            .build(mask)
            .map(|m| Compiled::Regex(Box::new(m)))
            .map_err(|e| Error::msg(format!("{mask:?} is not a valid regular expression: {e}"))),
    }
}

/// Does `name` match `matcher` as a regular expression?
///
/// **Unanchored**, because that is what the matcher was built as. The name is
/// searched as a slice rather than as a file, so nothing here opens anything.
///
/// A name containing a newline - legal on Linux - is searched line by line and
/// matches when any of its lines does, which is the same answer a search of a
/// one-line name gives and the only one a line-oriented engine can give.
pub fn matches_regex(matcher: &grep_regex::RegexMatcher, name: &str) -> bool {
    let mut searcher = grep_searcher::SearcherBuilder::new()
        // A name is one short line: no line numbers to count, and a buffer
        // that never has to hold more than one of them. `Some(0)` is refused
        // by the searcher as "no available searchers", so this is the smallest
        // limit it accepts.
        .line_number(false)
        .heap_limit(Some(NAME_HEAP_LIMIT))
        .build();
    let mut found = FirstMatch::default();
    // A search over an in-memory slice cannot fail for any reason the caller
    // could act on, and a name that could not be searched has not matched.
    searcher
        .search_slice(matcher, name.as_bytes(), &mut found)
        .is_ok()
        && found.hit
}

/// The searcher's buffer when it is matching one file name.
const NAME_HEAP_LIMIT: usize = 4096;

/// A [`grep_searcher::Sink`] that records whether anything matched and stops.
#[derive(Debug, Default)]
struct FirstMatch {
    hit: bool,
}

impl grep_searcher::Sink for FirstMatch {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        _mat: &grep_searcher::SinkMatch<'_>,
    ) -> std::result::Result<bool, Self::Error> {
        self.hit = true;
        // `false` stops the search: one match is the whole question.
        Ok(false)
    }
}

/// True when this mask matches everything, so a caller can skip the test
/// entirely on the overwhelmingly common `*` and `*.*`.
pub fn is_match_all(mask: &str) -> bool {
    let mask = mask.trim();
    mask.is_empty() || mask == "*" || mask == "*.*"
}

/// Does `name` match `mask`?
///
/// `mask` is the whole user-typed string: a list, optionally with a `|`
/// exclusion clause. An empty mask matches everything.
pub fn matches(mask: &str, name: &str) -> bool {
    if is_match_all(mask) {
        return true;
    }
    let (include, exclude) = match mask.split_once('|') {
        Some((inc, exc)) => (inc, exc),
        None => (mask, ""),
    };

    let included = if is_match_all(include) {
        true
    } else {
        patterns(include).any(|p| matches_one(p, name))
    };
    if !included {
        return false;
    }
    // An empty exclusion clause excludes nothing; `foo|` is `foo`.
    !patterns(exclude).any(|p| matches_one(p, name))
}

/// Split a mask list on whitespace, `;` and `,`, dropping empties.
fn patterns(list: &str) -> impl Iterator<Item = &str> {
    list.split([' ', '\t', ';', ','])
        .map(str::trim)
        .filter(|p| !p.is_empty())
}

/// Match one pattern against one name, case-insensitively.
///
/// Iterative with a single backtrack point, so a pathological pattern like
/// `*a*a*a*a*b` against a long name stays linear rather than exponential -
/// the classic recursive glob is not safe to point at a directory listing.
pub fn matches_one(pattern: &str, name: &str) -> bool {
    if pattern == "*.*" {
        // TC's "everything", not "names containing a dot".
        return true;
    }
    let p: Vec<char> = pattern.chars().flat_map(char::to_lowercase).collect();
    let n: Vec<char> = name.chars().flat_map(char::to_lowercase).collect();

    let (mut pi, mut ni) = (0usize, 0usize);
    // Where to resume if the current `*` turns out to have eaten too little.
    let mut star: Option<(usize, usize)> = None;

    while ni < n.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some((pi, ni));
                pi = pi.saturating_add(1);
            }
            Some('?') => {
                pi = pi.saturating_add(1);
                ni = ni.saturating_add(1);
            }
            // `n.get(ni)` rather than `n[ni]`: the loop condition already
            // proves `ni` is in range, and this way nothing has to be proved.
            Some(c) if n.get(ni) == Some(c) => {
                pi = pi.saturating_add(1);
                ni = ni.saturating_add(1);
            }
            // Mismatch: give the last `*` one more character and resume just
            // after it. The saved position must **advance**, or the loop
            // resumes at the same place for ever.
            _ => match star {
                Some((sp, sn)) => {
                    let next = sn.saturating_add(1);
                    pi = sp.saturating_add(1);
                    ni = next;
                    star = Some((sp, next));
                }
                None => return false,
            },
        }
    }
    // Trailing `*`s match the empty remainder.
    while p.get(pi) == Some(&'*') {
        pi = pi.saturating_add(1);
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_and_question_behave() {
        assert!(matches_one("*.rs", "main.rs"));
        assert!(!matches_one("*.rs", "main.rst"));
        assert!(matches_one("img_*.jpg", "img_001.jpg"));
        assert!(matches_one("?.txt", "a.txt"));
        assert!(!matches_one("?.txt", "ab.txt"));
        assert!(matches_one("*", ""));
        assert!(matches_one("a*", "a"));
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(matches("*.jpg", "PHOTO.JPG"));
        assert!(matches("*.JPG", "photo.jpg"));
    }

    #[test]
    fn tc_spells_everything_as_star_dot_star() {
        assert!(is_match_all("*.*"));
        assert!(is_match_all("*"));
        assert!(is_match_all(""));
        assert!(matches("*.*", "Makefile"), "a dotless name still matches");
    }

    #[test]
    fn a_list_matches_any_of_its_patterns() {
        assert!(matches("*.rs *.toml", "Cargo.toml"));
        assert!(matches("*.rs;*.toml", "main.rs"));
        assert!(!matches("*.rs,*.toml", "readme.md"));
    }

    #[test]
    fn a_pipe_excludes() {
        assert!(matches("*|*.bak", "notes.txt"));
        assert!(!matches("*|*.bak", "notes.bak"));
        assert!(
            matches("*.txt|", "notes.txt"),
            "an empty clause excludes nothing"
        );
    }

    #[test]
    fn a_pathological_pattern_does_not_blow_up() {
        let name = "a".repeat(64);
        // No match, and the matcher has to prove it without exploring 2^n
        // splits. A recursive glob would not return from this.
        assert!(!matches_one("*a*a*a*a*a*a*a*b", &name));
    }

    #[test]
    fn regex_masks_match_by_pattern() {
        // v0.2 deferred this mode; the `grep-regex` is what it was
        // waiting for, so both modes now work.
        assert!(MaskMode::Wildcard.unavailable().is_none());
        assert!(MaskMode::Regex.unavailable().is_none());

        let m = compile(r"\.rs$", MaskMode::Regex).expect("a valid regex");
        assert!(m.matches("main.rs"));
        assert!(!m.matches("main.rst"));
    }

    #[test]
    fn a_regex_mask_is_unanchored_and_case_insensitive() {
        // The two rules the module docs promise, and the escape from the
        // second one.
        let containing = compile("rs", MaskMode::Regex).expect("valid");
        assert!(containing.matches("cursor.txt"), "unanchored");

        let folded = compile(r"^main\.RS$", MaskMode::Regex).expect("valid");
        assert!(folded.matches("main.rs"), "case-insensitive by default");

        let exact = compile(r"(?-i)^main\.RS$", MaskMode::Regex).expect("valid");
        assert!(!exact.matches("main.rs"), "(?-i) restores case");
        assert!(exact.matches("main.RS"));
    }

    #[test]
    fn a_bad_regex_is_refused_with_its_reason() {
        let err = compile("*.rs", MaskMode::Regex).expect_err("a glob is not a regex");
        let text = err.to_string();
        assert!(text.contains("*.rs"), "{text}");
        assert!(text.contains("regular expression"), "{text}");
        // The same string in the other language is a perfectly good mask.
        assert!(
            compile("*.rs", MaskMode::Wildcard)
                .expect("wildcard")
                .matches("main.rs")
        );
    }

    #[test]
    fn a_compiled_wildcard_matches_what_the_free_function_does() {
        // One matcher, two entry points: the compiled form exists to avoid
        // recompiling per row, never to mean something different.
        for (mask, name) in [
            ("*.rs *.toml", "Cargo.toml"),
            ("*|*.bak", "notes.bak"),
            ("*.*", "Makefile"),
            ("?.txt", "ab.txt"),
        ] {
            let compiled = compile(mask, MaskMode::Wildcard).expect("wildcard");
            assert_eq!(compiled.matches(name), matches(mask, name), "{mask} {name}");
        }
    }
}
