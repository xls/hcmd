//! Finding web links in plain text.

use super::*;
use crate::config::ViewerConfig;

#[test]
fn the_link_under_a_byte_is_the_token_there_from_its_scheme() {
    let text = b"see https://x.invalid/a.pdf, then http://y.invalid/ and more";
    // Anywhere inside the first token finds it, from its start, without the
    // comma the sentence put after it.
    for at in [4usize, 10, 20, 26] {
        assert_eq!(
            url_span_at(text, at),
            Some((4, "https://x.invalid/a.pdf".to_string())),
            "at {at}"
        );
    }
    // The second, with its trailing slash kept: that is part of a URL.
    assert_eq!(
        url_span_at(text, 34),
        Some((34, "http://y.invalid/".to_string()))
    );
    // Prose is not a link.
    assert_eq!(url_span_at(text, 0), None);
    assert_eq!(url_span_at(text, 52), None);
    // Past the end is nothing.
    assert_eq!(url_span_at(text, text.len()), None);
}

#[test]
fn a_link_wrapped_in_brackets_or_glued_to_a_label_is_still_found() {
    assert_eq!(
        url_span_at(b"(https://x.invalid/p)", 5),
        Some((1, "https://x.invalid/p".to_string()))
    );
    assert_eq!(
        url_span_at(b"<http://x.invalid>", 3),
        Some((1, "http://x.invalid".to_string()))
    );
    // The scheme inside a token: the link starts at the scheme.
    assert_eq!(
        url_span_at(b"link:https://x.invalid/z", 2),
        Some((5, "https://x.invalid/z".to_string()))
    );
    // A bare scheme with nothing after it is not a link.
    assert_eq!(url_span_at(b"https://", 3), None);
    // Other schemes are not web links.
    assert_eq!(url_span_at(b"file:///etc/passwd", 4), None);
}

/// A text viewer over `body`.
fn text_viewer(body: &str) -> Viewer {
    let bytes = std::sync::Arc::new(body.as_bytes().to_vec());
    let len = bytes.len() as u64;
    let viewer = Viewer::open(
        crate::viewer::ViewerId(1),
        "notes.txt",
        None,
        crate::viewer::source::memory_opener(bytes),
        Some(len),
        &ViewerConfig::default(),
    )
    .expect("open");
    assert_eq!(viewer.mode(), ViewerMode::Text, "a .txt opens as text");
    viewer
}

#[test]
fn tab_steps_the_cursor_from_link_to_link_in_a_text_file() {
    let mut viewer = text_viewer(
        "intro line\nfirst: https://a.invalid/1.pdf here\nnothing ftp://x.invalid\nlast http://b.invalid/\n",
    );
    assert_eq!(
        viewer.url_under_cursor(),
        None,
        "the cursor starts on prose"
    );
    assert_eq!(
        viewer.step_text_link(true).expect("read"),
        Some("https://a.invalid/1.pdf".to_string())
    );
    assert_eq!(
        viewer.url_under_cursor(),
        Some("https://a.invalid/1.pdf".to_string()),
        "the cursor now stands on it"
    );
    // The ftp link is stepped over.
    assert_eq!(
        viewer.step_text_link(true).expect("read"),
        Some("http://b.invalid/".to_string())
    );
    // Nothing after the last one.
    assert_eq!(viewer.step_text_link(true).expect("read"), None);
    // And back again.
    assert_eq!(
        viewer.step_text_link(false).expect("read"),
        Some("https://a.invalid/1.pdf".to_string())
    );
    assert_eq!(viewer.step_text_link(false).expect("read"), None);
}

#[test]
fn the_link_under_the_cursor_is_underlined_in_its_row() {
    let mut viewer = text_viewer("first: https://a.invalid/1.pdf here\nplain line\n");
    viewer.step_text_link(true).expect("read");
    viewer.layout(10, 80).expect("layout");
    let underlined: Vec<String> = viewer
        .rows()
        .iter()
        .filter_map(|row| match row {
            crate::viewer::Row::Text { text, matches, .. } => matches
                .iter()
                .find(|m| m.underline && !m.current)
                .and_then(|m| text.get(m.range.clone()))
                .map(str::to_string),
            crate::viewer::Row::Hex { .. } => None,
        })
        .collect();
    assert_eq!(
        underlined,
        vec!["https://a.invalid/1.pdf".to_string()],
        "exactly the link, underlined, and not painted as a search hit"
    );
    // Off the link, nothing is underlined.
    viewer.goto_offset(0).expect("home");
    viewer.layout(10, 80).expect("layout");
    let any = viewer.rows().iter().any(|row| match row {
        crate::viewer::Row::Text { matches, .. } => matches.iter().any(|m| m.underline),
        crate::viewer::Row::Hex { .. } => false,
    });
    assert!(!any, "the cursor is on prose");
}
