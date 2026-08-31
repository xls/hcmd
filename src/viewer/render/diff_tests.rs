//! The diff renderer: what it prints, and where it folds.

use super::*;

/// The plain text of each rendered line.
fn text_of(rendered: &Rendered) -> Vec<String> {
    rendered.lines.iter().map(|l| l.text.clone()).collect()
}

#[test]
fn it_marks_the_two_sides_in_column_zero() {
    let old = "alpha\nbeta\ngamma\n";
    let new = "alpha\nBETA\ngamma\n";
    let out = render(old, new, "left", "right").expect("a diff");
    let lines = text_of(&out);
    assert_eq!(lines[0], "--- left");
    assert_eq!(lines[1], "+++ right");
    assert!(lines.contains(&"-beta".to_string()), "{lines:?}");
    assert!(lines.contains(&"+BETA".to_string()), "{lines:?}");
    // Column zero is reserved, so an unchanged line is a space and its own
    // text - never the text alone, which would make a line beginning with a
    // minus sign read as a deletion.
    assert!(lines.contains(&" alpha".to_string()), "{lines:?}");
}

#[test]
fn the_changed_lines_carry_the_diff_colours() {
    let out = render("a\n", "b\n", "l", "r").expect("a diff");
    let removed = out
        .lines
        .iter()
        .find(|l| l.text == "-a")
        .expect("the removed line");
    let added = out
        .lines
        .iter()
        .find(|l| l.text == "+b")
        .expect("the added line");
    assert_eq!(removed.spans[0].slot, Some(SynSlot::DiffRemoved));
    assert_eq!(added.spans[0].slot, Some(SynSlot::DiffAdded));
    assert_eq!(removed.spans[0].range, 0..removed.text.len());
}

#[test]
fn two_identical_files_say_so_rather_than_rendering_nothing() {
    let same = "alpha\nbeta\n";
    let out = render(same, same, "l", "r").expect("a diff");
    let lines = text_of(&out);
    assert!(
        lines.iter().any(|l| l.contains("identical")),
        "an empty diff is indistinguishable from a broken one: {lines:?}"
    );
}

#[test]
fn a_long_unchanged_run_is_folded_rather_than_printed() {
    // Forty identical lines between two changes. Three of context either side
    // are kept and the rest is folded.
    let mut old = String::from("start-old\n");
    let mut new = String::from("start-new\n");
    for i in 0..40 {
        old.push_str(&format!("line {i}\n"));
        new.push_str(&format!("line {i}\n"));
    }
    old.push_str("end-old\n");
    new.push_str("end-new\n");

    let out = render(&old, &new, "l", "r").expect("a diff");
    let folded = out
        .lines
        .iter()
        .find(|l| l.fold.is_some())
        .expect("a fold over the unchanged middle");
    let fold = folded.fold.as_ref().expect("checked");
    assert!(
        fold.summary.contains("34 unchanged lines"),
        "40 lines less three of context either side: {}",
        fold.summary
    );

    // Nothing is dropped: the folded lines are still in the document, which is
    // what makes expanding the fold show them.
    let kept: Vec<&String> = out.lines.iter().map(|l| &l.text).collect();
    assert!(kept.iter().any(|l| l.as_str() == " line 20"), "{kept:?}");
    // And the context either side of each change is there uncollapsed.
    assert!(kept.iter().any(|l| l.as_str() == " line 0"), "{kept:?}");
    assert!(kept.iter().any(|l| l.as_str() == " line 39"), "{kept:?}");
}

#[test]
fn a_short_unchanged_run_is_context_and_is_not_folded() {
    let old = "a\nx\nb\n";
    let new = "A\nx\nB\n";
    let out = render(old, new, "l", "r").expect("a diff");
    assert!(
        out.lines.iter().all(|l| l.fold.is_none()),
        "one line between two changes is context, not a fold"
    );
}

#[test]
fn a_file_with_no_trailing_newline_still_renders_its_last_line() {
    let out = render("a\nb", "a\nc", "l", "r").expect("a diff");
    let lines = text_of(&out);
    assert!(lines.contains(&"-b".to_string()), "{lines:?}");
    assert!(lines.contains(&"+c".to_string()), "{lines:?}");
}

#[test]
fn an_enormous_side_is_refused_rather_than_diffed() {
    let big = "x\n".repeat(MAX_LINES + 1);
    assert!(
        render(&big, "x\n", "l", "r").is_none(),
        "a diff is quadratic in the worst case and holds both sides as lines"
    );
}
