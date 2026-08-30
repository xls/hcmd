//! HTML as text: what survives, and what is dropped whole.

use super::*;

fn lines(text: &str) -> Vec<String> {
    render(text).into_iter().map(|l| l.text).collect()
}

const PAGE: &str = r#"<!DOCTYPE html>
<html>
<head>
  <title>Never shown</title>
  <style>body { color: red; }</style>
</head>
<body>
  <h1>Release notes</h1>
  <p>The viewer gained a third mode.</p>
  <script>
    var heading = "<h1>not a heading</h1>";
    if (a < b) { document.write("dropped"); }
  </script>
  <h2>Changes</h2>
  <ul>
    <li>JSON renders as a tree</li>
    <li>See <a href="https://example.invalid">the docs</a></li>
  </ul>
</body>
</html>"#;

#[test]
fn a_page_renders_as_its_text_with_the_structure_kept() {
    assert_eq!(
        lines(PAGE),
        vec![
            "Release notes",
            "",
            "The viewer gained a third mode.",
            "",
            "Changes",
            "",
            "  * JSON renders as a tree",
            "  * See the docs (https://example.invalid)",
        ]
    );
}

#[test]
fn a_script_is_dropped_whole_and_cannot_end_itself_early() {
    let out = lines(PAGE).join("\n");
    assert!(!out.contains("var heading"), "{out}");
    assert!(!out.contains("not a heading"), "{out}");
    assert!(!out.contains("document.write"), "{out}");
    assert!(!out.contains("dropped"), "{out}");
    // And the style block with it, contents included.
    assert!(!out.contains("color: red"), "{out}");
    // The title is inside `head`, which goes with them.
    assert!(!out.contains("Never shown"), "{out}");
}

#[test]
fn a_heading_carries_its_level() {
    let out = render("<h1>One</h1><h3>Three</h3><h6>Six</h6>");
    let texts: Vec<&str> = out.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(texts, vec!["One", "", "  Three", "", "        Six"]);
    // The top two levels take the heading slot and the deeper ones the type
    // slot, which is what makes a page's outline visible at a glance.
    assert_eq!(
        out.first()
            .and_then(|l| l.spans.first())
            .and_then(|s| s.slot),
        Some(SynSlot::Keyword)
    );
    assert_eq!(
        out.get(2)
            .and_then(|l| l.spans.first())
            .and_then(|s| s.slot),
        Some(SynSlot::Type)
    );
}

#[test]
fn the_markups_own_whitespace_is_collapsed() {
    // Newlines and indentation in the source are the author's editor
    // settings, not the page.
    assert_eq!(
        lines("<p>one\n   two\n\n\tthree</p>"),
        vec!["one two three"]
    );
}

#[test]
fn entities_are_decoded_for_the_ones_that_matter() {
    assert_eq!(
        lines("<p>a &amp; b &lt; c &gt; d &quot;e&quot; &nbsp; f</p>"),
        vec!["a & b < c > d \"e\" f"]
    );
    // An entity nothing knows is left as written rather than eaten.
    assert_eq!(lines("<p>&zzzz; and &amp;</p>"), vec!["&zzzz; and &"]);
    // A bare ampersand survives.
    assert_eq!(lines("<p>Tom &amp Jerry</p>"), vec!["Tom &amp Jerry"]);
}

#[test]
fn a_link_with_no_href_and_one_with_single_quotes_both_work() {
    assert_eq!(lines("<a>bare</a>"), vec!["bare"]);
    assert_eq!(
        lines("<a href='http://x.invalid'>t</a>"),
        vec!["t (http://x.invalid)"]
    );
    assert_eq!(lines("<a href=\"\">empty</a>"), vec!["empty"]);
}

#[test]
fn malformed_markup_renders_worse_rather_than_failing() {
    // An unclosed tag, a stray `<`, and a tag that never ends.
    assert_eq!(lines("<p>one<p>two"), vec!["one", "", "two"]);
    assert_eq!(lines("a < b and c"), vec!["a < b and c"]);
    // A `<` that never closes is not a tag, so it stays as the text it is.
    assert_eq!(lines("<p>text<unclosed"), vec!["text<unclosed"]);
    assert!(lines("").is_empty());
    assert!(lines("<html><body></body></html>").is_empty());
}

#[test]
fn a_table_row_starts_a_line_and_its_cells_are_spaced() {
    assert_eq!(
        lines("<table><tr><td>a</td><td>b</td></tr><tr><td>c</td></tr></table>"),
        vec!["a  b", "", "c"]
    );
}

#[test]
fn runs_of_blank_lines_are_one_and_the_page_does_not_end_in_them() {
    let out = lines("<div><div><div><p>deep</p></div></div></div>");
    assert_eq!(out, vec!["deep"]);
    assert!(!out.last().is_some_and(String::is_empty));
}

#[test]
fn nothing_rendered_ever_contains_a_newline_or_a_tab() {
    for line in render(PAGE) {
        assert!(!line.text.contains('\n'), "{:?}", line.text);
        assert!(!line.text.contains('\t'), "{:?}", line.text);
    }
}
