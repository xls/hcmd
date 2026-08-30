//! Display-width helpers and the ASCII/Unicode glyph table.
//!
//! Every non-ASCII character this crate draws is declared here, so
//! `ui.ascii_borders = true` has exactly one place to switch and there is one
//! place to audit. Truncation works on **grapheme clusters and display width**,
//! never on bytes or `char`s, so a CJK or emoji filename cannot
//! break column alignment or be cut mid-cluster.

use ratatui::symbols::border;
use unicode_segmentation::UnicodeSegmentation;

/// The glyph set for one session: box drawing and arrows, or their ASCII
/// counterparts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Glyphs {
    ascii: bool,
}

impl Glyphs {
    /// Build from `ui.ascii_borders`.
    pub const fn new(ascii: bool) -> Self {
        Self { ascii }
    }

    /// True when only ASCII may be emitted.
    pub const fn is_ascii(self) -> bool {
        self.ascii
    }

    /// The crop marker: `…`, or `...`.
    pub const fn ellipsis(self) -> &'static str {
        if self.ascii { "..." } else { "\u{2026}" }
    }

    /// Ascending sort arrow: `▲`, or `^`.
    pub const fn arrow_up(self) -> &'static str {
        if self.ascii { "^" } else { "\u{25B2}" }
    }

    /// Descending sort arrow: `▼`, or `v`.
    pub const fn arrow_down(self) -> &'static str {
        if self.ascii { "v" } else { "\u{25BC}" }
    }

    /// The arrow for a sort direction.
    pub const fn arrow(self, reverse: bool) -> &'static str {
        if reverse {
            self.arrow_down()
        } else {
            self.arrow_up()
        }
    }

    /// Horizontal rule fill: `─`, or `-`.
    pub const fn horizontal(self) -> &'static str {
        if self.ascii { "-" } else { "\u{2500}" }
    }

    /// The left junction of the rule that separates the entries from the panel
    /// status line: `├`, or `+`.
    pub const fn tee_left(self) -> &'static str {
        if self.ascii { "+" } else { "\u{251C}" }
    }

    /// The right junction: `┤`, or `+`.
    pub const fn tee_right(self) -> &'static str {
        if self.ascii { "+" } else { "\u{2524}" }
    }

    /// The vertical rule between tab-bar entries: `│`, or `|`.
    pub const fn vertical(self) -> &'static str {
        if self.ascii { "|" } else { "\u{2502}" }
    }

    /// The block cell used to paint an unfocused caret.
    ///
    /// ASCII has no solid block; `_` is the conventional stand-in and is what a
    /// VT100 renders for an underline cursor.
    pub const fn caret_block(self) -> &'static str {
        if self.ascii { "_" } else { "\u{2588}" }
    }

    /// The border set for a panel box.
    pub const fn border_set(self) -> border::Set<'static> {
        if self.ascii {
            border::Set {
                top_left: "+",
                top_right: "+",
                bottom_left: "+",
                bottom_right: "+",
                vertical_left: "|",
                vertical_right: "|",
                horizontal_top: "-",
                horizontal_bottom: "-",
            }
        } else {
            border::PLAIN
        }
    }
}

// ---------------------------------------------------------------------------
// Text measurement and cropping live in `crate::panel::text`.
//
// the rules - grapheme clusters, display width, and the middle crop
// that keeps a name's extension - are model behaviour, not painting: the panel
// status line has to know whether the entry under the cursor *was* cropped,
// which means the model computes the same thing the renderer
// draws. There was a second implementation here, and it disagreed with the
// model's about where a middle crop splits. These are forwards so there is one.
// ---------------------------------------------------------------------------

pub use crate::panel::text::{Crop, width};

/// The typographic characters hcmd puts in its own text, and their `ascii`
/// spellings (`ui.ascii_borders`).
///
/// Deliberately short, and deliberately only hcmd's own punctuation. It is
/// **not** a general transliteration: a file called `日本語.txt` or `café.jpg`
/// still renders as itself, because that is the user's data and mangling it
/// into `?` would be a worse bug than the one this fixes. What is downgraded
/// here is the chrome - the same category as the box-drawing characters and
/// the `…`/`▲`/`▼` the config already documents.
///
/// `§` has no ASCII spelling worth having: every use of it in this codebase is
/// `the design`, which reads correctly as `the design` once the sign is
/// dropped, and reads badly as `the design S13.2`.
const ASCII_SPELLINGS: &[(char, &str)] = &[
    ('\u{a7}', ""),      // § section sign
    ('\u{2014}', "-"),   // - em dash
    ('\u{2013}', "-"),   // - en dash
    ('\u{2026}', "..."), // … ellipsis
    ('\u{2192}', "->"),  // → rightwards arrow
    ('\u{2265}', ">="),  // ≥
    ('\u{2264}', "<="),  // ≤
    ('\u{d7}', "x"),     // × multiplication sign
    ('\u{b7}', "."),     // · middle dot
    ('\u{2018}', "'"),   // ' ' curly quotes
    ('\u{2019}', "'"),
    ('\u{201c}', "\""),
    ('\u{201d}', "\""),
];

/// Rewrite hcmd's own typographic characters as ASCII.
///
/// Applied to body text on its way to the screen when `ui.ascii_borders` is
/// set, so a message composed anywhere in the crate cannot put a glyph on a
/// VT100 that it has no way to draw. Borrowing when there is nothing to do
/// keeps it free for the overwhelmingly common case.
pub fn ascii_spelling(s: &str) -> std::borrow::Cow<'_, str> {
    if !s
        .chars()
        .any(|c| ASCII_SPELLINGS.iter().any(|(f, _)| *f == c))
    {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match ASCII_SPELLINGS.iter().find(|(f, _)| *f == c) {
            Some((_, to)) => out.push_str(to),
            None => out.push(c),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Crop `s` to at most `max` display columns, marking the cut with `ellipsis`.
///
/// Operates on grapheme clusters, so a cluster is never split. When `max` is
/// too small even for the ellipsis the string is hard-cropped instead, because
/// an ellipsis that does not fit is worse than a short name.
pub fn truncate(s: &str, max: usize, crop: Crop, ellipsis: &str) -> String {
    crate::panel::text::truncate_with(s, max, crop, ellipsis)
}

/// The longest grapheme prefix of `s` that fits in `max` columns.
pub fn take_front(s: &str, max: usize) -> String {
    crate::panel::text::clip(s, max)
}

/// The longest grapheme suffix of `s` that fits in `max` columns.
pub fn take_back(s: &str, max: usize) -> String {
    crate::panel::text::clip_start(s, max)
}

/// The substring of `s` occupying display columns `start..start + max`.
///
/// Used to scroll the command line horizontally. A grapheme straddling the
/// start is dropped rather than split. This one has no model counterpart -
/// horizontal scrolling is purely a painting concern.
pub fn slice_columns(s: &str, start: usize, max: usize) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    let mut used = 0usize;
    for g in s.graphemes(true) {
        let w = width(g);
        if col < start {
            // Anything before the cut, and a wide cluster straddling it, is
            // skipped whole.
            col = col.saturating_add(w);
            continue;
        }
        if used.saturating_add(w) > max {
            break;
        }
        used = used.saturating_add(w);
        col = col.saturating_add(w);
        out.push_str(g);
    }
    out
}

/// The first display column of the horizontal window that keeps a caret at
/// `caret_col` visible in a field `width` columns wide, with its own cell left
/// free.
///
/// The window is a pure function of the caret and the width, so no widget
/// needs to remember where it last scrolled to: `render` and `cursor` ask this
/// question separately and are guaranteed the same answer.
pub const fn caret_window(caret_col: usize, width: usize) -> usize {
    caret_col.saturating_sub(width.saturating_sub(1))
}

/// Crop to `w` columns and pad on the right, so the result occupies exactly
/// `w` cells.
pub fn fit_left(s: &str, w: usize, crop: Crop, ellipsis: &str) -> String {
    crate::panel::text::fit_with(s, w, crop, crate::panel::text::Align::Left, ellipsis)
}

/// Crop to `w` columns and pad on the left, for right-aligned columns.
pub fn fit_right(s: &str, w: usize, crop: Crop, ellipsis: &str) -> String {
    crate::panel::text::fit_with(s, w, crop, crate::panel::text::Align::Right, ellipsis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_strings_are_untouched() {
        assert_eq!(truncate("abc", 10, Crop::End, "…"), "abc");
        assert_eq!(truncate("abc", 3, Crop::Middle, "…"), "abc");
    }

    #[test]
    fn end_cropping_keeps_the_head() {
        assert_eq!(truncate("abcdefgh", 5, Crop::End, "…"), "abcd…");
        assert_eq!(truncate("abcdefgh", 5, Crop::End, "..."), "ab...");
    }

    #[test]
    fn middle_cropping_keeps_the_extension() {
        let got = truncate("some-very-long-report.tar.gz", 20, Crop::Middle, "…");
        assert_eq!(width(&got), 20);
        assert!(got.ends_with("tar.gz"), "{got}");
        assert!(got.starts_with("some"), "{got}");
    }

    #[test]
    fn wide_characters_never_overflow_or_split() {
        // Each of these is two columns wide.
        let s = "日本語のファイル名.txt";
        for max in 1..30usize {
            let got = truncate(s, max, Crop::End, "…");
            assert!(width(&got) <= max, "max={max} got={got:?}");
            let got = truncate(s, max, Crop::Middle, "…");
            assert!(width(&got) <= max, "max={max} got={got:?}");
        }
    }

    #[test]
    fn an_ellipsis_that_does_not_fit_is_dropped() {
        assert_eq!(truncate("abcdef", 2, Crop::End, "..."), "ab");
        assert_eq!(truncate("abcdef", 0, Crop::End, "…"), "");
    }

    #[test]
    fn fitting_produces_exact_widths() {
        assert_eq!(fit_left("ab", 5, Crop::End, "…"), "ab   ");
        assert_eq!(fit_right("ab", 5, Crop::End, "…"), "   ab");
        assert_eq!(width(&fit_left("abcdefgh", 5, Crop::End, "…")), 5);
    }

    #[test]
    fn slicing_by_column_scrolls_without_splitting() {
        assert_eq!(slice_columns("abcdef", 2, 3), "cde");
        assert_eq!(slice_columns("abcdef", 0, 100), "abcdef");
        assert_eq!(slice_columns("abcdef", 10, 3), "");
        let wide = "aa\u{65e5}bb";
        assert!(width(&slice_columns(wide, 3, 3)) <= 3);
    }

    #[test]
    fn every_glyph_has_an_ascii_counterpart() {
        let uni = Glyphs::new(false);
        let ascii = Glyphs::new(true);
        let pairs: [(&str, &str); 8] = [
            (uni.vertical(), ascii.vertical()),
            (uni.ellipsis(), ascii.ellipsis()),
            (uni.arrow_up(), ascii.arrow_up()),
            (uni.arrow_down(), ascii.arrow_down()),
            (uni.horizontal(), ascii.horizontal()),
            (uni.tee_left(), ascii.tee_left()),
            (uni.tee_right(), ascii.tee_right()),
            (uni.caret_block(), ascii.caret_block()),
        ];
        for (u, a) in pairs {
            assert!(!a.is_empty());
            assert!(a.is_ascii(), "{a:?} is not ASCII");
            assert_ne!(u, a);
        }
        let set = ascii.border_set();
        for s in [
            set.top_left,
            set.top_right,
            set.bottom_left,
            set.bottom_right,
            set.vertical_left,
            set.vertical_right,
            set.horizontal_top,
            set.horizontal_bottom,
        ] {
            assert!(s.is_ascii(), "{s:?} is not ASCII");
        }
    }

    /// under `ui.ascii_borders` hcmd's own punctuation has an ASCII
    /// spelling - and the user's filenames are left exactly as they are.
    #[test]
    fn ascii_spelling_downgrades_chrome_and_leaves_data_alone() {
        // The two that the messages actually emit.
        assert_eq!(
            ascii_spelling("payload/x: refused \u{2014} outside dest"),
            "payload/x: refused - outside dest"
        );
        assert_eq!(ascii_spelling("a \u{2192} b"), "a -> b");
        assert_eq!(
            ascii_spelling("2 \u{d7} 1.1 \u{2265} free"),
            "2 x 1.1 >= free"
        );

        // A filename is the user's, not ours.
        for name in [
            "\u{65e5}\u{672c}\u{8a9e}.txt",
            "caf\u{e9}.jpg",
            "\u{df}.bin",
        ] {
            assert_eq!(ascii_spelling(name), name, "a filename was mangled");
        }

        // Nothing to do is not a copy.
        assert!(matches!(
            ascii_spelling("plain ascii"),
            std::borrow::Cow::Borrowed(_)
        ));
    }
}
