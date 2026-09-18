//! Selecting the rendered text.

use super::*;
use crate::config::{ViewerConfig, ViewerMode};
use crate::viewer::copy::{Copied, CopyRequest};

/// `DOC` rendered as Markdown: three lines, the middle one blank.
const DOC: &str = "one two\n\nthree four\n";

fn rendered(body: &str) -> Viewer {
    let bytes = std::sync::Arc::new(body.as_bytes().to_vec());
    let len = bytes.len() as u64;
    let mut viewer = Viewer::open(
        crate::viewer::ViewerId(1),
        "doc.md",
        None,
        crate::viewer::source::memory_opener(bytes),
        Some(len),
        &ViewerConfig::default(),
    )
    .expect("open");
    viewer.set_mode(ViewerMode::Render).expect("mode 3");
    assert_eq!(viewer.mode(), ViewerMode::Render);
    viewer
}

fn shift(viewer: &mut Viewer, motion: Motion, times: usize) {
    for _ in 0..times {
        viewer.move_render_sel(motion, Extend::Linear);
    }
}

#[test]
fn shift_right_selects_characters_and_copies_exactly_them() {
    let mut viewer = rendered(DOC);
    assert!(viewer.render_selection().is_none(), "nothing until Shift");
    shift(&mut viewer, Motion::Right, 3);
    assert_eq!(viewer.render_selection(), Some(((0, 0), (0, 3))));
    assert_eq!(viewer.render_row_sel(0), Some(0..3));
    assert_eq!(viewer.render_row_sel(1), None);
    assert_eq!(viewer.rendered_selection_text().as_deref(), Some("one"));
    // Ctrl+C copies the drawn text, not the file's bytes.
    match viewer.copy(CopyRequest::Selection, 1024).expect("copy") {
        Copied::Text { text, .. } => assert_eq!(text, "one"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_selection_over_lines_joins_them_with_newlines() {
    let mut viewer = rendered(DOC);
    shift(&mut viewer, Motion::Right, 3);
    // Down takes the rest of the first line and keeps the column where it
    // can: the blank line has none.
    shift(&mut viewer, Motion::Down, 1);
    assert_eq!(viewer.render_selection(), Some(((0, 0), (1, 0))));
    assert_eq!(
        viewer.rendered_selection_text().as_deref(),
        Some("one two\n")
    );
    shift(&mut viewer, Motion::Down, 1);
    // Back onto a line with text, the column is where it was: three in.
    assert_eq!(viewer.render_selection(), Some(((0, 0), (2, 3))));
    assert_eq!(
        viewer.rendered_selection_text().as_deref(),
        Some("one two\n\nthr")
    );
}

#[test]
fn a_plain_move_drops_the_selection_and_esc_clears_it() {
    let mut viewer = rendered(DOC);
    shift(&mut viewer, Motion::Right, 2);
    assert!(viewer.render_selection().is_some());
    viewer.move_render_sel(Motion::Right, Extend::None);
    assert!(
        viewer.render_selection().is_none(),
        "an unshifted arrow drops it"
    );
    assert_eq!(viewer.render_col(), 3);

    shift(&mut viewer, Motion::Right, 2);
    assert!(viewer.clear_render_selection(), "there was one to clear");
    assert!(!viewer.clear_render_selection(), "and now there is not");
    // Through the shared entry the dispatcher uses.
    shift(&mut viewer, Motion::Right, 1);
    assert!(viewer.clear_selection());
}

#[test]
fn left_and_right_cross_line_ends_and_home_end_are_the_lines_ends() {
    let mut viewer = rendered(DOC);
    viewer.move_render_sel(Motion::RowEnd, Extend::None);
    assert_eq!(viewer.render_col(), "one two".len());
    // Right at the end of a line steps onto the next line's start.
    viewer.move_render_sel(Motion::Right, Extend::None);
    assert_eq!((viewer.render_cursor, viewer.render_col()), (1, 0));
    // Left at the start of a line steps back to the previous line's end.
    viewer.move_render_sel(Motion::Left, Extend::None);
    assert_eq!(
        (viewer.render_cursor, viewer.render_col()),
        (0, "one two".len())
    );
    viewer.move_render_sel(Motion::RowStart, Extend::None);
    assert_eq!(viewer.render_col(), 0);
}

#[test]
fn select_all_takes_the_whole_document() {
    let mut viewer = rendered(DOC);
    viewer.select_all_rendered();
    assert_eq!(
        viewer.rendered_selection_text().as_deref(),
        Some("one two\n\nthree four")
    );
    // And through the mode-aware entry the key uses.
    viewer.clear_selection();
    viewer.select_all();
    assert_eq!(
        viewer.render_selection(),
        Some(((0, 0), (2, "three four".len())))
    );
}

#[test]
fn the_rows_carry_the_selection_and_the_cursor_column() {
    let mut viewer = rendered(DOC);
    shift(&mut viewer, Motion::Right, 3);
    viewer.layout(10, 80).expect("layout");
    let first = viewer.rows().first().expect("a row");
    match first {
        crate::viewer::Row::Text { sel, cursor, .. } => {
            assert_eq!(sel, &Some(0..3));
            assert_eq!(cursor, &Some(3), "the hardware cursor follows the column");
        }
        crate::viewer::Row::Hex { .. } => panic!("mode 3 draws text rows"),
    }
}
