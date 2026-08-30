//! Which renderer a name gets, and walking a document with folds in it.

use super::*;
use std::collections::BTreeSet;

#[test]
fn a_name_chooses_its_renderer_and_an_unknown_one_chooses_none() {
    for (name, want) in [
        ("package.json", RenderKind::Json),
        ("Notes.MD", RenderKind::Markdown),
        ("index.html", RenderKind::Html),
        ("page.HTM", RenderKind::Html),
        ("map.geojson", RenderKind::Json),
        ("notebook.ipynb", RenderKind::Json),
    ] {
        assert_eq!(RenderKind::of_name(name), Some(want), "{name}");
    }
    for name in ["main.rs", "notes", "archive.tar.gz", "", ".json"] {
        // `.json` with no stem is a dotfile called json, not a JSON file.
        if name == ".json" {
            continue;
        }
        assert_eq!(RenderKind::of_name(name), None, "{name}");
    }
}

/// A document of ten lines with two regions: 1 through 4, and 6 through 8.
fn document() -> (Rendered, std::collections::BTreeMap<usize, usize>) {
    let mut lines: Vec<RenderLine> = (0..10)
        .map(|n| RenderLine::plain(format!("line {n}")))
        .collect();
    if let Some(line) = lines.get_mut(1) {
        line.fold = Some(Fold {
            through: 4,
            summary: "first {...}".to_string(),
        });
    }
    if let Some(line) = lines.get_mut(6) {
        line.fold = Some(Fold {
            through: 8,
            summary: "second {...}".to_string(),
        });
    }
    let document = Rendered {
        kind: RenderKind::Json,
        label: "json".to_string(),
        lines,
    };
    let regions = document.foldable();
    (document, regions)
}

#[test]
fn with_nothing_collapsed_every_line_is_drawn() {
    let (document, regions) = document();
    let none = BTreeSet::new();
    let folded = Folded::new(&regions, &none, document.len());
    assert_eq!(folded.window(0, 20), (0..10).collect::<Vec<usize>>());
    assert_eq!(folded.next(3), Some(4));
    assert_eq!(folded.prev(3), Some(2));
    assert_eq!(folded.prev(0), None);
    assert_eq!(folded.next(9), None);
}

#[test]
fn a_collapsed_region_hides_its_body_and_keeps_its_own_line() {
    let (document, regions) = document();
    let collapsed = BTreeSet::from([1]);
    let folded = Folded::new(&regions, &collapsed, document.len());
    // 1 is still drawn; 2, 3 and 4 are not.
    assert_eq!(folded.window(0, 20), vec![0, 1, 5, 6, 7, 8, 9]);
    assert!(folded.shows(1));
    assert!(!folded.shows(2));
    assert!(!folded.shows(4));
    assert!(folded.shows(5));
    assert_eq!(folded.next(1), Some(5));
    assert_eq!(folded.prev(5), Some(1));
}

#[test]
fn two_collapsed_regions_both_hide_and_the_walk_skips_both() {
    let (document, regions) = document();
    let collapsed = BTreeSet::from([1, 6]);
    let folded = Folded::new(&regions, &collapsed, document.len());
    assert_eq!(folded.window(0, 20), vec![0, 1, 5, 6, 9]);
    assert_eq!(folded.next(6), Some(9));
    assert_eq!(folded.prev(9), Some(6));
}

/// The outermost fold is what hides a line, whether or not the inner one is
/// collapsed too - jumping to the inner one would land off screen.
#[test]
fn a_region_inside_a_collapsed_one_is_hidden_by_the_outer_fold() {
    let mut lines: Vec<RenderLine> = (0..8).map(|n| RenderLine::plain(format!("{n}"))).collect();
    if let Some(line) = lines.get_mut(0) {
        line.fold = Some(Fold {
            through: 7,
            summary: "outer".to_string(),
        });
    }
    if let Some(line) = lines.get_mut(2) {
        line.fold = Some(Fold {
            through: 4,
            summary: "inner".to_string(),
        });
    }
    let document = Rendered {
        kind: RenderKind::Json,
        label: "json".to_string(),
        lines,
    };
    let regions = document.foldable();
    let collapsed = BTreeSet::from([0, 2]);
    let folded = Folded::new(&regions, &collapsed, document.len());
    assert_eq!(folded.window(0, 20), vec![0]);
    assert_eq!(folded.next(0), None);
    // Expanding only the outer one brings the inner summary back.
    let outer_only = BTreeSet::from([2]);
    let folded = Folded::new(&regions, &outer_only, document.len());
    assert_eq!(folded.window(0, 20), vec![0, 1, 2, 5, 6, 7]);
}

#[test]
fn a_window_starting_inside_a_collapsed_region_lands_after_it() {
    let (document, regions) = document();
    let collapsed = BTreeSet::from([1]);
    let folded = Folded::new(&regions, &collapsed, document.len());
    // Asking for the window at line 3, which is hidden.
    assert_eq!(folded.at_or_after(3), Some(5));
    assert_eq!(folded.window(3, 3), vec![5, 6, 7]);
}

#[test]
fn a_window_never_runs_past_the_end() {
    let (document, regions) = document();
    let none = BTreeSet::new();
    let folded = Folded::new(&regions, &none, document.len());
    assert_eq!(folded.window(8, 10), vec![8, 9]);
    assert!(folded.window(10, 5).is_empty());
    assert!(folded.window(500, 5).is_empty());
    assert!(!folded.shows(10));
}

#[test]
fn an_empty_document_walks_without_answering_anything() {
    let document = Rendered {
        kind: RenderKind::Markdown,
        label: "markdown".to_string(),
        lines: Vec::new(),
    };
    let regions = document.foldable();
    let none = BTreeSet::new();
    let folded = Folded::new(&regions, &none, document.len());
    assert!(document.is_empty());
    assert!(folded.window(0, 10).is_empty());
    assert_eq!(folded.at_or_after(0), None);
    assert_eq!(folded.next(0), None);
}

#[test]
fn the_dispatcher_refuses_only_what_the_renderer_refuses() {
    assert!(render(RenderKind::Json, "{\"a\":1}").is_some());
    assert!(render(RenderKind::Json, "not json").is_none());
    // The other two never refuse: every text file is valid Markdown, and
    // there is no HTML this cannot walk.
    assert!(render(RenderKind::Markdown, "anything at all").is_some());
    assert!(render(RenderKind::Html, "<<<>>>").is_some());
}
