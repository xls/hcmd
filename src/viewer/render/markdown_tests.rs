//! Markdown rendered: headings, lists, quotes, code and inline markers.

use super::*;

fn lines(text: &str) -> Vec<String> {
    render(text).into_iter().map(|l| l.text).collect()
}

const DOC: &str = "# Installing\n\
                   \n\
                   Run the **installer**, then check the *version*.\n\
                   \n\
                   ## Steps\n\
                   \n\
                   - check the version\n\
                   - put it on the PATH\n\
                   \n\
                   1. first\n\
                   2. second\n\
                   \n\
                   > a quoted remark\n\
                   \n\
                   ```rust\n\
                   let x = 1;\n\
                   ```\n\
                   \n\
                   See [the manual](https://example.invalid/docs).\n";

#[test]
fn a_document_renders_its_blocks_rather_than_their_markers() {
    assert_eq!(
        lines(DOC),
        vec![
            "INSTALLING",
            "==========",
            "",
            "Run the installer, then check the version.",
            "",
            "Steps",
            "-----",
            "",
            "  * check the version",
            "  * put it on the PATH",
            "",
            "  1. first",
            "  2. second",
            "",
            "  | a quoted remark",
            "",
            "    [rust]",
            "    let x = 1;",
            "",
            "See the manual (https://example.invalid/docs).",
        ]
    );
}

#[test]
fn a_top_level_heading_is_shouted_and_ruled_and_a_deep_one_is_indented() {
    assert_eq!(lines("# One"), vec!["ONE", "==="]);
    assert_eq!(lines("## Two"), vec!["Two", "---"]);
    // Below the second level there is no rule, only indentation: a rule under
    // every `####` would be more decoration than document.
    assert_eq!(lines("### Three"), vec!["  Three"]);
    assert_eq!(lines("###### Six"), vec!["        Six"]);
    // Seven hashes is not a heading in Markdown and is not one here.
    assert_eq!(lines("####### Seven"), vec!["####### Seven"]);
}

#[test]
fn inside_a_fence_nothing_is_markup() {
    let out = lines("```\n# not a heading\n- not a list\n**not bold**\n```\n");
    assert_eq!(
        out,
        vec![
            "    # not a heading",
            "    - not a list",
            "    **not bold**",
        ]
    );
    // A tilde fence closes on tildes and not on backticks.
    assert_eq!(lines("~~~\n# kept\n~~~\n"), vec!["    # kept"]);
}

#[test]
fn an_unclosed_fence_runs_to_the_end_rather_than_losing_the_rest() {
    assert_eq!(
        lines("text\n```\nstill code\nmore code\n"),
        vec!["text", "    still code", "    more code"]
    );
}

#[test]
fn emphasis_code_and_links_lose_their_markers_and_keep_their_text() {
    assert_eq!(lines("a **bold** b"), vec!["a bold b"]);
    assert_eq!(lines("a __bold__ b"), vec!["a bold b"]);
    assert_eq!(lines("a *italic* b"), vec!["a italic b"]);
    assert_eq!(lines("a `code` b"), vec!["a code b"]);
    // A link keeps its target, which is the half a reader cannot get back.
    assert_eq!(
        lines("see [docs](http://x.invalid)"),
        vec!["see docs (http://x.invalid)"]
    );
    // A bare reference has no target to show.
    assert_eq!(lines("see [docs] now"), vec!["see docs now"]);
}

#[test]
fn a_lone_marker_is_left_alone_rather_than_eating_the_line() {
    assert_eq!(lines("2 * 3 = 6"), vec!["2 * 3 = 6"]);
    assert_eq!(lines("a * b"), vec!["a * b"]);
    assert_eq!(lines("an unclosed **bold"), vec!["an unclosed **bold"]);
    assert_eq!(lines("snake_case_name"), vec!["snake_case_name"]);
}

#[test]
fn the_rendered_text_carries_the_slots_that_make_it_readable() {
    let out = render("# Title\n\nsome **bold** text\n");
    let slot = |at: usize| {
        out.get(at)
            .and_then(|l: &RenderLine| l.spans.first())
            .and_then(|s| s.slot)
    };
    assert_eq!(slot(0), Some(SynSlot::Keyword), "the heading");
    assert_eq!(slot(1), Some(SynSlot::Punctuation), "its rule");
    assert_eq!(slot(3), Some(SynSlot::Keyword), "the emphasis");
}

#[test]
fn nested_list_indentation_is_the_files_own() {
    assert_eq!(
        lines("- one\n  - two\n    - three\n"),
        vec!["  * one", "    * two", "      * three"]
    );
}

#[test]
fn a_thematic_break_is_a_rule_and_a_dashed_line_is_not_a_list() {
    let out = lines("above\n\n---\n\nbelow\n");
    assert_eq!(out.get(2).map(String::len), Some(RULE_WIDTH));
    assert_eq!(out.first().map(String::as_str), Some("above"));
    assert_eq!(out.last().map(String::as_str), Some("below"));
}

#[test]
fn a_file_with_no_markup_in_it_comes_back_as_itself() {
    let plain = "just a line\nand another\n";
    assert_eq!(lines(plain), vec!["just a line", "and another"]);
    assert!(lines("").is_empty());
}

#[test]
fn an_indented_block_is_code_and_a_blank_line_is_kept() {
    assert_eq!(
        lines("text\n\n    indented code\n\nmore\n"),
        vec!["text", "", "    indented code", "", "more"]
    );
}

#[test]
fn nothing_rendered_ever_contains_a_newline_or_a_tab() {
    // The row is the unit the viewer draws; a line holding either would
    // silently become two rows or throw the indentation out.
    for line in render(DOC) {
        assert!(!line.text.contains('\n'), "{:?}", line.text);
        assert!(!line.text.contains('\t'), "{:?}", line.text);
    }
}
