//! Walking the links in a rendered document.

use super::*;
use crate::config::{ViewerConfig, ViewerMode};

/// A viewer over `body`, named so the renderer is chosen by extension.
fn open_named(name: &str, body: &str, cfg: &ViewerConfig) -> Viewer {
    let bytes = std::sync::Arc::new(body.as_bytes().to_vec());
    let len = bytes.len() as u64;
    Viewer::open(
        crate::viewer::ViewerId(1),
        name,
        None,
        crate::viewer::source::memory_opener(bytes),
        Some(len),
        cfg,
    )
    .expect("open")
}

/// Three links over two rendered lines, with a page among the files.
const DOC: &str = "Intro [one](https://a.invalid/1.pdf) text\n\n\
                   More [two](https://b.invalid/) and [three](https://c.invalid/3.zip)\n";

/// `DOC` in mode 3.
fn rendered(body: &str) -> Viewer {
    let cfg = ViewerConfig::default();
    let mut viewer = open_named("doc.md", body, &cfg);
    viewer.set_mode(ViewerMode::Render).expect("mode 3");
    assert_eq!(
        viewer.mode(),
        ViewerMode::Render,
        "the fixture has to reach mode 3 for any of this to mean anything"
    );
    viewer
}

#[test]
fn tab_walks_the_links_in_order_and_wraps_at_both_ends() {
    let mut viewer = rendered(DOC);
    assert!(
        viewer.focused_link().is_none(),
        "nothing is focused until Tab"
    );
    assert_eq!(
        viewer.step_link(true).as_deref(),
        Some("https://a.invalid/1.pdf")
    );
    assert_eq!(
        viewer.focused_link().map(|link| link.target.as_str()),
        Some("https://a.invalid/1.pdf")
    );
    assert_eq!(
        viewer.step_link(true).as_deref(),
        Some("https://b.invalid/")
    );
    assert_eq!(
        viewer.step_link(true).as_deref(),
        Some("https://c.invalid/3.zip")
    );
    // Off the end wraps to the first...
    assert_eq!(
        viewer.step_link(true).as_deref(),
        Some("https://a.invalid/1.pdf")
    );
    // ...and backwards from the first wraps to the last.
    assert_eq!(
        viewer.step_link(false).as_deref(),
        Some("https://c.invalid/3.zip")
    );
}

#[test]
fn the_cursor_follows_the_link_and_leaving_its_line_unfocuses_it() {
    let mut viewer = rendered(DOC);
    viewer.step_link(true);
    viewer.step_link(true);
    assert!(
        viewer.render_cursor > 0,
        "the second link is on a later line"
    );
    assert!(viewer.focused_link().is_some());
    // Moving the cursor off the line is how Enter goes back to meaning fold.
    viewer.move_render(crate::viewer::Motion::Up);
    assert!(viewer.focused_link().is_none());
}

#[test]
fn a_document_with_no_links_has_nothing_to_focus() {
    let mut viewer = rendered("just words, no links\n");
    assert_eq!(viewer.step_link(true), None);
    assert_eq!(viewer.step_link(false), None);
    assert!(viewer.focused_link().is_none());
}

#[test]
fn the_focused_link_is_painted_like_the_current_match() {
    let mut viewer = rendered(DOC);
    viewer.step_link(true);
    let line = viewer.render_cursor;
    let runs = viewer.render_matches_on(line);
    let run = runs.first().expect("the focused link is a run");
    assert_eq!(runs.len(), 1, "no search is on, so it is the only run");
    assert!(run.current, "in the current-match colour");
    let text = viewer
        .rendered()
        .and_then(|doc| doc.lines.get(line))
        .map(|l| l.text.as_str())
        .expect("the line");
    assert_eq!(
        text.get(run.range.clone()),
        Some("one"),
        "it covers the label"
    );
    // Another line has no run from it.
    assert!(viewer.render_matches_on(line.saturating_add(1)).is_empty());
}
