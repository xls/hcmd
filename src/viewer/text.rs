//! Turning a decoded line into rows on the screen.
//!
//! > **Text** - line-based, optional wrap, line numbers, tabs expanded to
//! > `viewer.tab_width`, syntax highlighting.
//!
//! Everything here is a pure function of one line's text and a width. It is
//! deliberately separate from the streaming core: a line arrives as bytes from
//! a window, is decoded once by [`crate::viewer::decode`], and is turned into
//! rows here. Nothing in this module knows how big the file is.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

use crate::panel::text::width;
use crate::viewer::highlight::Span;

/// The longest single line the text renderer will lay out (the design's
/// memory rule, applied to a file with no line breaks in it).
///
/// A 40 GB file with no `\n` is one line, and laying it out would be laying out
/// the file. Beyond this the line is cut and the cut is *shown* - the viewer
/// says the line continues rather than pretending it ended.
pub const MAX_LINE_BYTES: u64 = 64 * 1024;

/// The widest `viewer.tab_width` that is honoured.
///
/// **This is a memory bound, not a taste one** - the same bound
/// [`crate::viewer::hex::MAX_HEX_WIDTH`] is, and it is here for the same
/// reason. Expansion costs one space per column, so one row costs
/// `line bytes × tab_width`: at `viewer.tab_width = 65535` a line of
/// [`MAX_LINE_BYTES`] tabs expands to **3.9 GB**, which is the design's
/// "memory is capped and is a function of the window size" broken by a number
/// in a config file. Clamped, the worst case is
/// `rows × MAX_LINE_BYTES × MAX_TAB_WIDTH` - a constant times the terminal,
/// with the file's size nowhere in it.
///
/// Sixteen because it is past the widest stop any editor offers and well past
/// the eight `expand(1)` uses, so no reachable preference is being refused.
pub const MAX_TAB_WIDTH: u16 = 16;

/// The marker put at the end of a line that was cut at [`MAX_LINE_BYTES`].
pub const CUT_MARK: &str = "\u{21b4}";

/// The `ui.ascii_borders` stand-in for [`CUT_MARK`].
///
/// `>` is what a pager puts in the right margin of a line that runs off the
/// screen, and it is one cell wide on every terminal.
pub const CUT_MARK_ASCII: &str = ">";

/// The cut marker for this session's glyph set.
pub const fn cut_mark(ascii: bool) -> &'static str {
    if ascii { CUT_MARK_ASCII } else { CUT_MARK }
}

/// The marker for a line that runs past the right edge with wrap off.
///
/// A different fact from [`CUT_MARK`]: this line continues and `Right` will
/// show it, whereas a cut line has been given up on. They collapse to the same
/// ASCII glyph, which is the price of ASCII.
pub const MORE_MARK: &str = "\u{00bb}";

/// The `ui.ascii_borders` stand-in for [`MORE_MARK`].
pub const MORE_MARK_ASCII: &str = ">";

/// The "continues to the right" marker for this session's glyph set.
pub const fn more_mark(ascii: bool) -> &'static str {
    if ascii { MORE_MARK_ASCII } else { MORE_MARK }
}

/// The marker for a line whose start is off the left edge, with wrap off.
///
/// It is drawn in the gutter's separating column rather than in the text, so
/// horizontal scrolling costs no column that the file could have used.
pub const LESS_MARK: &str = "\u{00ab}";

/// The `ui.ascii_borders` stand-in for [`LESS_MARK`].
pub const LESS_MARK_ASCII: &str = "<";

/// The "starts off the left edge" marker for this session's glyph set.
pub const fn less_mark(ascii: bool) -> &'static str {
    if ascii { LESS_MARK_ASCII } else { LESS_MARK }
}

/// The `ui.ascii_borders` stand-in for a C0 control or DEL.
///
/// A Control Picture is a glyph *this program* chose to stand in for a byte,
/// exactly as [`CUT_MARK`] and [`MORE_MARK`] are, so it degrades with them: on
/// the terminal `ui.ascii_borders` exists for, U+2400 arrives as a replacement
/// box or as mojibake, and a multi-byte glyph landing in one cell shifts the
/// rest of the row. One column, so the column accounting is unchanged.
pub const CONTROL_MARK_ASCII: char = '?';

/// Expand tabs to `tab_width` stops and make control characters visible.
///
///
/// Tabs advance to the next multiple of `tab_width` measured in *display
/// columns*, which is what a tab means - expanding each to a fixed run of
/// spaces would put the columns of a file mixing tabs and spaces in the wrong
/// places.
///
/// A C0 control other than tab becomes its Unicode Control Picture
/// (`U+2400` plus the code point), so a `\x07` is one visible cell rather than
/// a terminal bell rung by drawing a file - or [`CONTROL_MARK_ASCII`] when
/// `ascii` says the terminal cannot draw one.
///
/// A lone `\r` - the residue of a CRLF whose `\n` has already been trimmed, or
/// a classic-Mac line ending - is dropped, because emitting it would move the
/// terminal's cursor to the start of the row.
pub fn expand(line: &str, tab_width: u16, ascii: bool) -> String {
    expand_tracked(line, tab_width, ascii, &[]).0
}

/// [`expand`], plus where a set of byte offsets in `line` ended up.
///
/// `wants` must be sorted and holds byte offsets into `line`; the returned
/// vector is the same length and holds each one's offset in the expanded
/// string. This exists because highlight spans are byte ranges into the
/// **decoded** line while the screen shows the **expanded** one, and a tab that
/// became four spaces moves everything after it - mapping by clamping instead
/// paints a Go file's keywords one column short for every tab on the line.
///
/// An offset that falls *inside* a grapheme cluster rounds **up** to the end of
/// that cluster. Rounding is what keeps the answer on a character boundary, and
/// rounding up rather than down keeps the mapping non-decreasing, so a span
/// never comes back inverted.
pub fn expand_tracked(
    line: &str,
    tab_width: u16,
    ascii: bool,
    wants: &[usize],
) -> (String, Vec<usize>) {
    // Clamped here rather than at the call site, so there is one place a tab
    // stop can come from and no caller can spend more memory than
    // [`MAX_TAB_WIDTH`] allows - the rule
    // [`crate::viewer::hex::HexLayout::new`] enforces for `viewer.hex_width`.
    let tab = usize::from(tab_width.clamp(1, MAX_TAB_WIDTH));
    let mut out = String::with_capacity(line.len());
    let mut mapped: Vec<usize> = Vec::with_capacity(wants.len());
    let mut col = 0_usize;
    for (idx, g) in line.grapheme_indices(true) {
        while mapped.len() < wants.len() && wants.get(mapped.len()).is_some_and(|w| *w <= idx) {
            mapped.push(out.len());
        }
        match g {
            "\t" => {
                let stop = col.saturating_add(tab).saturating_sub(col % tab);
                let pad = stop.saturating_sub(col);
                for _ in 0..pad {
                    out.push(' ');
                }
                col = stop;
            }
            "\r" | "\u{feff}" => {}
            _ => {
                let mut chars = g.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) if (c as u32) < 0x20 || c as u32 == 0x7F => {
                        let pic = if ascii {
                            CONTROL_MARK_ASCII
                        } else if c as u32 == 0x7F {
                            '\u{2421}'
                        } else {
                            char::from_u32(0x2400_u32.saturating_add(c as u32))
                                .unwrap_or('\u{fffd}')
                        };
                        out.push(pic);
                        col = col.saturating_add(1);
                    }
                    _ => {
                        col = col.saturating_add(width(g));
                        out.push_str(g);
                    }
                }
            }
        }
    }
    while mapped.len() < wants.len() {
        mapped.push(out.len());
    }
    (out, mapped)
}

/// Expand a line and carry its highlight spans across the expansion.
///
///
/// The spans that come back are byte ranges into the returned string, which is
/// what the renderer needs: it has the row's text and nothing else. Empty spans
/// are dropped - they paint nothing - and the order is preserved.
pub fn expand_spans(
    line: &str,
    tab_width: u16,
    ascii: bool,
    spans: &[Span],
) -> (String, Vec<Span>) {
    if spans.is_empty() {
        return (expand(line, tab_width, ascii), Vec::new());
    }
    let mut wants: Vec<usize> = Vec::with_capacity(spans.len().saturating_mul(2));
    for s in spans {
        wants.push(s.range.start);
        wants.push(s.range.end);
    }
    wants.sort_unstable();
    wants.dedup();
    let (out, mapped) = expand_tracked(line, tab_width, ascii, &wants);
    let at = |x: usize| -> usize {
        wants
            .binary_search(&x)
            .ok()
            .and_then(|i| mapped.get(i).copied())
            .unwrap_or(out.len())
    };
    let moved = spans
        .iter()
        .filter_map(|s| {
            let start = at(s.range.start);
            let end = at(s.range.end).max(start);
            (start < end).then_some(Span {
                range: start..end,
                slot: s.slot,
            })
        })
        .collect();
    (out, moved)
}

/// Clip spans to one row's byte range and rebase them onto it.
///
/// A wrapped line is highlighted once and drawn in pieces (the design's
/// "optional wrap" against the line-oriented parser), and horizontal scrolling
/// shows a different piece of the same line. Both are this function: the
/// ranges that survive are the ones the row can see, expressed from the start
/// of the row.
pub fn slice_spans(spans: &[Span], range: &Range<usize>) -> Vec<Span> {
    spans
        .iter()
        .filter_map(|s| {
            let start = s.range.start.max(range.start);
            let end = s.range.end.min(range.end);
            (start < end).then(|| Span {
                range: start.saturating_sub(range.start)..end.saturating_sub(range.start),
                slot: s.slot,
            })
        })
        .collect()
}

/// The byte range of `text` occupying display columns `skip..skip + take`.
///
/// This is horizontal scrolling with wrap off. A grapheme
/// cluster straddling either edge is dropped whole rather than split - half of
/// a wide character is not a character - which is the same rule the command
/// line scrolls by ([`crate::ui::text::slice_columns`]), so the two never
/// disagree about where a column is.
///
/// The range is always a valid byte range into `text` and is empty when the
/// scroll has run past the end of the line.
pub fn column_range(text: &str, skip: usize, take: usize) -> Range<usize> {
    let mut col = 0_usize;
    let mut used = 0_usize;
    let mut start: Option<usize> = None;
    let mut end = text.len();
    for (idx, g) in text.grapheme_indices(true) {
        let w = width(g).max(1);
        if col < skip {
            // Wholly before the cut, or straddling it: skipped either way.
            col = col.saturating_add(w);
            continue;
        }
        if start.is_none() {
            start = Some(idx);
        }
        if used.saturating_add(w) > take {
            end = idx;
            break;
        }
        used = used.saturating_add(w);
        col = col.saturating_add(w);
    }
    match start {
        Some(s) => s..end.max(s),
        None => text.len()..text.len(),
    }
}

/// Break an expanded line into rows of at most `columns` display columns.
///
/// With wrap off (the "optional wrap") the caller does not call
/// this: it takes the one row and scrolls it horizontally. With wrap on, this
/// is the whole of it. Grapheme clusters are never split, and a cluster wider
/// than the whole row gets a row of its own rather than an infinite loop.
///
/// Returns byte ranges into `line` rather than owned strings, so highlighting
/// can be computed once for the line and sliced per row.
pub fn wrap(line: &str, columns: usize) -> Vec<std::ops::Range<usize>> {
    if columns == 0 || line.is_empty() {
        return std::iter::once(0..line.len()).collect();
    }
    let mut rows = Vec::new();
    let mut start = 0_usize;
    let mut col = 0_usize;
    for (idx, g) in line.grapheme_indices(true) {
        let w = width(g).max(1);
        if col.saturating_add(w) > columns && idx > start {
            rows.push(start..idx);
            start = idx;
            col = 0;
        }
        col = col.saturating_add(w);
    }
    rows.push(start..line.len());
    rows
}

/// How many rows an expanded line takes at `columns` wide, with wrap on.
pub fn row_count(line: &str, columns: usize) -> usize {
    wrap(line, columns).len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::viewer::highlight::SynSlot;

    fn span(range: Range<usize>, slot: Option<SynSlot>) -> Span {
        Span { range, slot }
    }

    #[test]
    fn tabs_go_to_the_next_stop_not_to_a_fixed_run_of_spaces() {
        assert_eq!(expand("a\tb", 4, false), "a   b");
        assert_eq!(expand("ab\tc", 4, false), "ab  c");
        assert_eq!(expand("abcd\te", 4, false), "abcd    e");
        assert_eq!(expand("\tx", 8, false), "        x");
        assert_eq!(expand("a\tb", 1, false), "a b");
        assert_eq!(
            expand("a\tb", 0, false),
            "a b",
            "a zero tab width does not hang"
        );
    }

    #[test]
    fn a_tab_stop_cannot_spend_more_memory_than_the_window_rule_allows() {
        // "memory is capped and is a function of the window
        // size, not the file size". Expansion costs one space per column, so an
        // unclamped `viewer.tab_width` makes it a function of a config value
        // instead: 60,000 tabs at 65,535 columns each is 3.9 GB for one row,
        // and thirty seconds spent building it.
        let line: String = std::iter::repeat_n('\t', 60_000).collect();
        let out = expand(&line, u16::MAX, false);
        assert_eq!(
            out.len(),
            60_000 * usize::from(MAX_TAB_WIDTH),
            "the widest tab stop honoured is MAX_TAB_WIDTH, whatever was asked for"
        );
        // And every stop up to the cap is still exactly what was asked for.
        assert_eq!(expand("a\tb", MAX_TAB_WIDTH, false).len(), 1 + 15 + 1);
    }

    #[test]
    fn a_wide_character_counts_its_columns_against_the_tab_stop() {
        // 日 is two columns, so the tab has two columns left to fill.
        assert_eq!(expand("日\tx", 4, false), "日  x");
    }

    #[test]
    fn control_characters_become_pictures_rather_than_being_obeyed() {
        assert_eq!(expand("a\x07b", 4, false), "a\u{2407}b");
        assert_eq!(expand("a\x00b", 4, false), "a\u{2400}b");
        assert_eq!(expand("a\x7fb", 4, false), "a\u{2421}b");
        assert_eq!(
            expand("a\rb", 4, false),
            "ab",
            "a stray CR would move the cursor"
        );
        assert_eq!(expand("\u{feff}hi", 4, false), "hi");
    }

    #[test]
    fn ascii_borders_reaches_the_control_pictures_too() {
        // `ui.ascii_borders` is for the terminal that cannot draw
        // the glyphs *this program* chose. A Control Picture is one of those,
        // exactly as the cut and more marks are.
        let out = expand("a\x0cb\x07c\x00d\x7fe", 4, true);
        assert_eq!(out, "a?b?c?d?e");
        assert!(out.is_ascii());
        // And one column each, so nothing after it shifts.
        assert_eq!(
            expand("a\x0cb", 4, true).chars().count(),
            expand("a\x0cb", 4, false).chars().count()
        );
    }

    #[test]
    fn wrapping_never_splits_a_cluster_and_never_loops() {
        assert_eq!(wrap("abcdef", 3), vec![0..3, 3..6]);
        assert_eq!(wrap("", 3), vec![0..0]);
        assert_eq!(
            wrap("abc", 0),
            vec![0..3],
            "zero columns is one row, not none"
        );
        // 日本 is four columns; at three per row it takes two rows and the
        // cluster is not cut in half.
        let rows = wrap("日本", 3);
        assert_eq!(rows.len(), 2);
        assert_eq!(&"日本"[rows[0].clone()], "日");
        // A cluster wider than the row still gets a row rather than spinning.
        assert_eq!(wrap("日", 1).len(), 1);
        assert_eq!(row_count("abcdefgh", 3), 3);
    }

    #[test]
    fn expansion_carries_spans_with_it_rather_than_leaving_them_behind() {
        // the spans are byte ranges into the decoded line; a tab
        // that became four spaces moves everything after it.
        let line = "\tlet x = 1;";
        let spans = vec![
            span(1..4, Some(SynSlot::Keyword)), // "let"
            span(9..10, Some(SynSlot::Number)), // "1"
        ];
        let (text, moved) = expand_spans(line, 4, false, &spans);
        assert_eq!(text, "    let x = 1;");
        assert_eq!(text.get(moved[0].range.clone()), Some("let"));
        assert_eq!(text.get(moved[1].range.clone()), Some("1"));
        assert_eq!(moved[0].slot, Some(SynSlot::Keyword));
    }

    #[test]
    fn a_span_boundary_inside_a_cluster_rounds_rather_than_splitting_it() {
        // The 'e' plus a combining acute is one cluster of three bytes; a span
        // ending in the middle of it must not produce a byte index that would
        // panic a slice.
        let line = "e\u{301}x";
        let spans = vec![span(0..2, Some(SynSlot::String))];
        let (text, moved) = expand_spans(line, 4, false, &spans);
        assert_eq!(text, line);
        assert!(text.get(moved[0].range.clone()).is_some(), "{moved:?}");
        assert!(moved[0].range.end >= moved[0].range.start);
    }

    #[test]
    fn spans_survive_being_sliced_onto_a_wrapped_row() {
        let spans = vec![span(0..4, Some(SynSlot::Keyword)), span(4..10, None)];
        let row = slice_spans(&spans, &(3..8));
        assert_eq!(row.len(), 2);
        assert_eq!(row[0].range, 0..1);
        assert_eq!(row[0].slot, Some(SynSlot::Keyword));
        assert_eq!(row[1].range, 1..5);
        // A row entirely past the spans keeps nothing rather than clamping
        // everything onto column zero.
        assert!(slice_spans(&spans, &(20..30)).is_empty());
    }

    #[test]
    fn horizontal_scrolling_slices_on_columns_and_never_splits_a_cluster() {
        let line = "abcdefghij";
        assert_eq!(&line[column_range(line, 0, 4)], "abcd");
        assert_eq!(&line[column_range(line, 4, 4)], "efgh");
        assert_eq!(&line[column_range(line, 8, 4)], "ij");
        assert_eq!(&line[column_range(line, 40, 4)], "");
        assert_eq!(&line[column_range(line, 0, 0)], "");
        // 日 is two columns: scrolling one column past it drops it whole rather
        // than showing half of it.
        let wide = "\u{65e5}ab";
        assert_eq!(&wide[column_range(wide, 1, 4)], "ab");
        assert_eq!(&wide[column_range(wide, 0, 1)], "");
        for skip in 0..8 {
            for take in 0..8 {
                let r = column_range(wide, skip, take);
                assert!(wide.get(r.clone()).is_some(), "{skip} {take} {r:?}");
                assert!(width(&wide[r]) <= take);
            }
        }
    }

    #[test]
    fn wrapping_covers_the_line_exactly_once() {
        let line = "the quick brown fox jumps over the lazy dog";
        for columns in 1..20 {
            let rows = wrap(line, columns);
            let rebuilt: String = rows.iter().map(|r| &line[r.clone()]).collect();
            assert_eq!(rebuilt, line, "columns = {columns}");
        }
    }
}
