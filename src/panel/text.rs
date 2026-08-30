//! Grapheme-aware text fitting for panel columns.
//!
//! Every operation here works on **grapheme clusters and display width**, never
//! on bytes or `char`s. A CJK name is twice as wide per cluster as an ASCII one
//! and an emoji with a variation selector or a skin-tone modifier is several
//! `char`s in one cluster; cutting on either of the wrong units breaks column
//! alignment or produces a mangled cluster, and the design rules both out.
//!
//! Nothing in this module can panic: every slice goes through `get`, and every
//! subtraction is guarded or saturating.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// The Unicode horizontal-ellipsis used to mark a cropped string.
pub const ELLIPSIS: &str = "\u{2026}";

/// The `ui.ascii_borders` replacement for [`ELLIPSIS`].
pub const ELLIPSIS_ASCII: &str = "...";

/// Which ellipsis to use, given `ui.ascii_borders`.
pub const fn ellipsis(ascii: bool) -> &'static str {
    if ascii { ELLIPSIS_ASCII } else { ELLIPSIS }
}

/// Which part of an over-long string is thrown away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Crop {
    /// Keep the head: `some-very-long-project-nam…`. Used when the `ext` column
    /// is rendered, because the informative tail is already shown there.
    End,
    /// Keep both ends: `some-very-long-…report.tar.gz`. Used when `ext` is
    /// hidden or not configured, because then the extension lives inside the
    /// name and is the most informative part of it.
    Middle,
}

/// Horizontal alignment of a rendered cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Align {
    /// Padding goes on the right.
    Left,
    /// Padding goes on the left. The `size` column.
    Right,
}

/// Display width of a string in terminal cells.
pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// The longest prefix of `s`, in whole grapheme clusters, that fits in `cells`.
///
/// Never splits a cluster, and never returns something wider than `cells`.
pub fn clip(s: &str, cells: usize) -> String {
    if width(s) <= cells {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for cluster in s.graphemes(true) {
        let w = width(cluster);
        if used.saturating_add(w) > cells {
            break;
        }
        used = used.saturating_add(w);
        out.push_str(cluster);
    }
    out
}

/// The longest suffix of `s`, in whole grapheme clusters, that fits in `cells`.
pub fn clip_start(s: &str, cells: usize) -> String {
    if width(s) <= cells {
        return s.to_string();
    }
    let clusters: Vec<&str> = s.graphemes(true).collect();
    let mut used = 0usize;
    let mut start = clusters.len();
    while start > 0 {
        let Some(cluster) = clusters.get(start.saturating_sub(1)) else {
            break;
        };
        let w = width(cluster);
        if used.saturating_add(w) > cells {
            break;
        }
        used = used.saturating_add(w);
        start = start.saturating_sub(1);
    }
    clusters.get(start..).unwrap_or(&[]).concat()
}

/// Crop `s` to at most `cells` display columns, marking the cut with an
/// ellipsis.
///
/// * A string that already fits is returned untouched.
/// * When `cells` leaves no room for the ellipsis *and* a cluster, the string is
///   hard-clipped instead - an ellipsis alone tells the reader nothing they
///   could not see from the column being empty.
/// * The result is never *wider* than `cells`, but it can be narrower by one
///   cell when the cut falls next to a double-width cluster. Pad with [`fit`].
pub fn truncate(s: &str, cells: usize, crop: Crop, ascii: bool) -> String {
    truncate_with(s, cells, crop, ellipsis(ascii))
}

/// [`truncate`] with the ellipsis given directly rather than chosen from
/// `ascii`.
///
/// The renderer picks its glyphs from one table (`ui::text::Glyphs`) and has a
/// `&str` in hand, not a `bool`; this is the primitive both spellings call so
/// there is one cropping implementation rather than two.
pub fn truncate_with(s: &str, cells: usize, crop: Crop, mark: &str) -> String {
    if width(s) <= cells {
        return s.to_string();
    }
    if cells == 0 {
        return String::new();
    }
    let mark_width = width(mark);
    if cells <= mark_width {
        return clip(s, cells);
    }
    let avail = cells.saturating_sub(mark_width);

    match crop {
        Crop::End => {
            let mut out = clip(s, avail);
            out.push_str(mark);
            out
        }
        Crop::Middle => {
            let clusters: Vec<&str> = s.graphemes(true).collect();
            let tail_budget = avail / 2;
            let head_budget = avail.saturating_sub(tail_budget);

            // Head, by cluster index, so the tail can be told where to stop.
            let mut head_end = 0usize;
            let mut head_used = 0usize;
            while let Some(cluster) = clusters.get(head_end) {
                let w = width(cluster);
                if head_used.saturating_add(w) > head_budget {
                    break;
                }
                head_used = head_used.saturating_add(w);
                head_end = head_end.saturating_add(1);
            }

            // Anything the head did not spend goes to the tail, so a head made
            // entirely of double-width clusters does not waste a cell.
            let tail_budget = tail_budget.saturating_add(head_budget.saturating_sub(head_used));
            let mut tail_start = clusters.len();
            let mut tail_used = 0usize;
            while tail_start > head_end {
                let Some(cluster) = clusters.get(tail_start.saturating_sub(1)) else {
                    break;
                };
                let w = width(cluster);
                if tail_used.saturating_add(w) > tail_budget {
                    break;
                }
                tail_used = tail_used.saturating_add(w);
                tail_start = tail_start.saturating_sub(1);
            }

            let mut out = clusters.get(..head_end).unwrap_or(&[]).concat();
            out.push_str(mark);
            out.push_str(&clusters.get(tail_start..).unwrap_or(&[]).concat());
            out
        }
    }
}

/// Pad `s` to exactly `cells` display columns, hard-clipping anything longer.
pub fn pad(s: &str, cells: usize, align: Align) -> String {
    let w = width(s);
    if w > cells {
        return clip(s, cells);
    }
    let fill = " ".repeat(cells.saturating_sub(w));
    match align {
        Align::Left => {
            let mut out = String::with_capacity(s.len().saturating_add(fill.len()));
            out.push_str(s);
            out.push_str(&fill);
            out
        }
        Align::Right => {
            let mut out = String::with_capacity(s.len().saturating_add(fill.len()));
            out.push_str(&fill);
            out.push_str(s);
            out
        }
    }
}

/// Crop and pad in one step: the result is **always** exactly `cells` display
/// columns wide, so a row of cells lines up whatever is in it.
pub fn fit(s: &str, cells: usize, crop: Crop, align: Align, ascii: bool) -> String {
    fit_with(s, cells, crop, align, ellipsis(ascii))
}

/// [`fit`] with the ellipsis given directly.
pub fn fit_with(s: &str, cells: usize, crop: Crop, align: Align, mark: &str) -> String {
    pad(&truncate_with(s, cells, crop, mark), cells, align)
}

/// True when `s` does not fit in `cells` and would therefore be cropped.
///
/// The panel status line shows the full name of the entry under the cursor when
/// this is true.
pub fn is_cropped(s: &str, cells: usize) -> bool {
    width(s) > cells
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A family emoji: one grapheme cluster, seven `char`s, four scalar
    /// values joined by ZWJ.
    const FAMILY: &str = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
    /// `e` + combining acute: one cluster, two `char`s, width 1.
    const COMBINING: &str = "cafe\u{301}";
    const CJK: &str = "日本語のファイル名.txt";

    #[test]
    fn a_string_that_fits_is_untouched() {
        assert_eq!(truncate("short.rs", 20, Crop::End, false), "short.rs");
        assert_eq!(truncate("short.rs", 8, Crop::Middle, false), "short.rs");
    }

    #[test]
    fn end_cropping_appends_the_ellipsis() {
        assert_eq!(
            truncate("some-very-long-project-name", 12, Crop::End, false),
            "some-very-l\u{2026}"
        );
        assert_eq!(
            truncate("some-very-long-project-name", 12, Crop::End, true),
            "some-very..."
        );
    }

    #[test]
    fn ascii_ellipsis_never_overflows_the_budget() {
        for cells in 0..40usize {
            let out = truncate("some-very-long-project-name", cells, Crop::End, true);
            assert!(width(&out) <= cells, "{cells}: {out:?}");
        }
    }

    #[test]
    fn middle_cropping_keeps_the_extension() {
        let out = truncate("some-very-long-report.tar.gz", 20, Crop::Middle, false);
        assert!(out.ends_with(".tar.gz"), "{out}");
        assert!(out.starts_with("some"), "{out}");
        assert!(width(&out) <= 20);
    }

    #[test]
    fn cjk_never_overflows_and_never_splits_a_cluster() {
        for cells in 0..30usize {
            for crop in [Crop::End, Crop::Middle] {
                let out = truncate(CJK, cells, crop, false);
                assert!(width(&out) <= cells, "{cells}: {out:?}");
                // Every cluster in the output is a whole cluster of the input
                // or the ellipsis.
                for cluster in out.graphemes(true) {
                    assert!(
                        cluster == ELLIPSIS || CJK.graphemes(true).any(|c| c == cluster),
                        "{cluster:?} is not a whole cluster of the input"
                    );
                }
            }
        }
    }

    #[test]
    fn an_emoji_cluster_is_never_cut_in_half() {
        let name = format!("{FAMILY}{FAMILY}{FAMILY}-photo.png");
        for cells in 0..30usize {
            let out = truncate(&name, cells, Crop::End, false);
            assert!(width(&out) <= cells);
            assert!(
                !out.contains('\u{200D}') || out.contains(FAMILY),
                "a ZWJ escaped without its cluster: {out:?}"
            );
        }
    }

    #[test]
    fn combining_marks_travel_with_their_base() {
        // Width 4, so it fits in 4 and is cropped at 3.
        assert_eq!(width(COMBINING), 4);
        assert_eq!(truncate(COMBINING, 4, Crop::End, false), COMBINING);
        let out = truncate(COMBINING, 3, Crop::End, false);
        assert!(width(&out) <= 3);
        assert!(!out.starts_with('\u{301}'));
    }

    #[test]
    fn fit_is_always_exactly_the_requested_width() {
        for cells in 0..25usize {
            for s in ["a", CJK, COMBINING, FAMILY, "", "0123456789"] {
                for align in [Align::Left, Align::Right] {
                    let out = fit(s, cells, Crop::Middle, align, false);
                    assert_eq!(width(&out), cells, "{s:?} at {cells} -> {out:?}");
                }
            }
        }
    }

    #[test]
    fn right_alignment_pads_on_the_left() {
        assert_eq!(fit("42", 6, Crop::End, Align::Right, false), "    42");
        assert_eq!(fit("42", 6, Crop::End, Align::Left, false), "42    ");
    }

    #[test]
    fn a_budget_too_small_for_the_ellipsis_hard_clips() {
        assert_eq!(truncate("abcdef", 1, Crop::End, false), "a");
        assert_eq!(truncate("abcdef", 0, Crop::End, false), "");
        // "..." is three cells wide, so at three cells there is no room for it
        // plus content.
        assert_eq!(truncate("abcdef", 3, Crop::End, true), "abc");
    }
}
