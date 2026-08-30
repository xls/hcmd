//! Mode 3's find: over the document, and over nothing else.

use super::*;
use crate::config::ViewerConfig;

/// A viewer over `body`, named so the renderer is chosen by extension.
fn open_named(name: &str, body: &str, cfg: &ViewerConfig) -> Viewer {
    let bytes = std::sync::Arc::new(body.as_bytes().to_vec());
    let len = bytes.len() as u64;
    Viewer::open(
        ViewerId(1),
        name,
        None,
        source::memory_opener(bytes),
        Some(len),
        cfg,
    )
    .expect("open")
}

/// Two `needle`s, on two different rendered lines.
const DOC: &str = r#"{"alpha":1,"beta":"needle","gamma":[1,2,3],"delta":"needle again"}"#;

/// A viewer in mode 3 with `query` typed into the find bar.
fn searching(query: &str) -> Viewer {
    let cfg = ViewerConfig::default();
    let mut viewer = open_named("doc.json", DOC, &cfg);
    viewer
        .set_mode(crate::config::ViewerMode::Render)
        .expect("mode 3");
    assert_eq!(
        viewer.mode(),
        crate::config::ViewerMode::Render,
        "the fixture has to reach mode 3 for any of this to mean anything"
    );
    viewer.open_find();
    for ch in query.chars() {
        viewer.find_type(ch).expect("typing searches");
    }
    viewer
}

#[test]
fn it_finds_every_match_in_the_rendered_text() {
    let viewer = searching("needle");
    assert_eq!(
        viewer.render_find_counter(),
        Some((1, 2)),
        "both `needle`s are on screen and both are counted"
    );
}

#[test]
fn the_count_is_exact_rather_than_a_running_total() {
    // The streaming search reports `0+` while its background scan is still
    // going. Mode 3 reads the whole document in one pass, so there is no
    // interim answer to report and the bar never carries a `+`.
    let viewer = searching("needle");
    let bar = viewer.find_bar_text();
    assert!(bar.contains("1/2"), "{bar}");
    assert!(!bar.contains('+'), "the count is not provisional: {bar}");
}

#[test]
fn n_steps_through_the_matches_and_wraps() {
    let mut viewer = searching("needle");
    assert_eq!(viewer.render_find_counter(), Some((1, 2)));
    viewer.find_next().expect("step");
    assert_eq!(viewer.render_find_counter(), Some((2, 2)));
    // Wraps, because the whole document was searched: "no match below" and
    // "no match" are the same statement here.
    viewer.find_next().expect("wrap");
    assert_eq!(viewer.render_find_counter(), Some((1, 2)));
    viewer.find_prev().expect("back");
    assert_eq!(viewer.render_find_counter(), Some((2, 2)));
}

#[test]
fn the_cursor_lands_on_the_line_the_match_is_on() {
    let mut viewer = searching("needle");
    let first = viewer.render_cursor();
    viewer.find_next().expect("step");
    let second = viewer.render_cursor();
    assert!(
        second > first,
        "stepping did not move to the later match: {first} then {second}"
    );
}

#[test]
fn it_matches_text_the_file_does_not_contain_in_that_form() {
    // The point of searching the document rather than the file. The rendered
    // view writes `beta: "needle"`; the file has `"beta":"needle"`, with no
    // space and an extra quote, so this pattern exists only on screen.
    let viewer = searching("beta: ");
    assert_eq!(
        viewer.render_find_counter(),
        Some((1, 1)),
        "mode 3 searches what it draws"
    );
}

#[test]
fn leaving_mode_three_forgets_hits_that_no_longer_mean_anything() {
    let mut viewer = searching("needle");
    assert!(viewer.render_find_counter().is_some());
    viewer
        .set_mode(crate::config::ViewerMode::Text)
        .expect("mode 1");
    assert_eq!(
        viewer.render_find_counter(),
        None,
        "a line number into a document that is no longer drawn highlights rows \
         by coincidence"
    );
}

#[test]
fn a_seeded_pattern_finds_its_matches_without_a_mode_round_trip() {
    // The session's last search is installed compiled but not run. Mode 3 then
    // held a live pattern and an empty hit list, so the first `n` reported a
    // word that is on screen as not found - and going to mode 1 and back
    // "fixed" it, because a mode change rebuilds the hits.
    let cfg = ViewerConfig::default();
    let mut viewer = open_named("doc.json", DOC, &cfg);
    viewer
        .set_mode(crate::config::ViewerMode::Render)
        .expect("mode 3");
    viewer.seed_find(crate::viewer::find::FindQuery {
        input: "needle".to_string(),
        ..crate::viewer::find::FindQuery::default()
    });
    let found = viewer.find_next().expect("step");
    assert!(
        !matches!(found, crate::viewer::find::Found::None),
        "a seeded pattern that is on screen was reported as not found"
    );
    assert_eq!(viewer.render_find_counter(), Some((1, 2)));
}

#[test]
fn opening_the_bar_empties_it() {
    let mut viewer = searching("needle");
    assert_eq!(viewer.render_find_counter(), Some((1, 2)));
    viewer.clear_find().expect("clear");
    assert!(
        viewer.find_query().input.is_empty(),
        "Ctrl+F left the old pattern in the bar"
    );
    assert_eq!(
        viewer.render_find_counter(),
        None,
        "no pattern means nothing on screen matches it"
    );
}
