//! A unified diff, as a rendered document.
//!
//! Mode 3 renders a document; a diff is one. It is **not** a fourth viewer
//! mode, and the reason it is not is that [`RenderLine`] already carries
//! everything a diff needs: `spans` are the colour runs that make `+` green
//! and `-` red, and `fold` is what collapses a long run of unchanged lines
//! behind `24 unchanged lines`. `1` and `2` still give the file's own text and
//! bytes, because mode 3 leaves the byte cursor where it was, and mode 3's
//! find searches the rendered text, so searching a diff needs nothing here.
//!
//! # The shape
//!
//! Unified, because unified is the format everybody can already read and the
//! one `diff -u`, `git diff` and every review tool print. Column zero is
//! reserved: a space for context, `-` for the left side only, `+` for the
//! right side only. That reservation is what makes the colouring trivial and
//! unambiguous - a context line that begins with a minus sign is still a
//! space followed by a minus sign.
//!
//! Runs of unchanged lines longer than twice [`CONTEXT`] are folded rather
//! than dropped. `diff -u` drops them and prints `@@` to say where it jumped
//! to; this keeps them, collapsed, because a viewer can afford to and because
//! "show me the bit you hid" is one keystroke on a fold.

use similar::{ChangeTag, TextDiff};

use super::{Fold, RenderKind, RenderLine, Rendered};
use crate::viewer::highlight::{Span, SynSlot};

/// How many unchanged lines are kept either side of a change.
///
/// Three is what `diff -u` uses by default, and the number every reader of a
/// diff already has their eye trained on.
pub const CONTEXT: usize = 3;

/// The most lines either side may have before this refuses.
///
/// A diff is quadratic in the worst case and both sides are held in memory as
/// lines. Mode 3 already refuses a file past `viewer.render_max`; this is the
/// same bound expressed in the unit this algorithm actually costs in.
pub const MAX_LINES: usize = 200_000;

/// One whole-line span in `slot`.
fn whole(text: &str, slot: SynSlot) -> Vec<Span> {
    vec![Span {
        range: 0..text.len(),
        slot: Some(slot),
    }]
}

/// A line of the diff, coloured by what it is.
fn line(text: String, slot: SynSlot) -> RenderLine {
    let spans = whole(&text, slot);
    RenderLine {
        text,
        spans,
        fold: None,
    }
}

/// Render `old` against `new` as a unified diff.
///
/// `None` when either side is too long to diff, which the caller reports as a
/// refusal rather than as an empty document.
#[must_use]
pub fn render(old: &str, new: &str, old_label: &str, new_label: &str) -> Option<Rendered> {
    if old.lines().count() > MAX_LINES || new.lines().count() > MAX_LINES {
        return None;
    }
    let diff = TextDiff::from_lines(old, new);

    let mut lines: Vec<RenderLine> = Vec::new();
    lines.push(line(format!("--- {old_label}"), SynSlot::DiffHeader));
    lines.push(line(format!("+++ {new_label}"), SynSlot::DiffHeader));

    // Every change, in order, with the unchanged runs between them measured so
    // that a long one can be folded instead of printed.
    let changes: Vec<_> = diff.iter_all_changes().collect();
    let mut index = 0_usize;
    let mut identical = true;
    while index < changes.len() {
        let Some(change) = changes.get(index) else {
            break;
        };
        if change.tag() != ChangeTag::Equal {
            identical = false;
            let text = change.value().trim_end_matches(['\n', '\r']).to_string();
            let (mark, slot) = match change.tag() {
                ChangeTag::Delete => ('-', SynSlot::DiffRemoved),
                ChangeTag::Insert => ('+', SynSlot::DiffAdded),
                ChangeTag::Equal => (' ', SynSlot::DiffMarker),
            };
            lines.push(line(format!("{mark}{text}"), slot));
            index = index.saturating_add(1);
            continue;
        }

        // A run of unchanged lines. How it is drawn depends only on how long
        // it is: short enough and it is context, longer and its middle is
        // folded away behind a line saying how much.
        let start = index;
        while changes
            .get(index)
            .is_some_and(|c| c.tag() == ChangeTag::Equal)
        {
            index = index.saturating_add(1);
        }
        let run = &changes[start..index];
        let leading = start == 0;
        let trailing = index >= changes.len();
        emit_equal(&mut lines, run, leading, trailing);
    }

    if identical {
        lines.push(RenderLine::plain(String::new()));
        lines.push(line(
            "The two sides are identical.".to_string(),
            SynSlot::DiffMarker,
        ));
    }

    Some(Rendered {
        kind: RenderKind::Diff,
        label: RenderKind::Diff.label().to_string(),
        lines,
    })
}

/// Draw one run of unchanged lines.
///
/// Keeps [`CONTEXT`] either side of it and folds the middle. A run at the very
/// start or the very end of the document has only one side that needs context,
/// which is why the two flags are passed rather than inferred: three lines of
/// context above the first change is useful, and three lines above nothing is
/// three lines of a file the reader did not ask to see.
fn emit_equal(
    lines: &mut Vec<RenderLine>,
    run: &[similar::Change<&str>],
    leading: bool,
    trailing: bool,
) {
    let text_of = |change: &similar::Change<&str>| {
        format!(" {}", change.value().trim_end_matches(['\n', '\r']))
    };
    let before = if leading { 0 } else { CONTEXT };
    let after = if trailing { 0 } else { CONTEXT };

    if run.len() <= before.saturating_add(after) {
        for change in run {
            lines.push(RenderLine::plain(text_of(change)));
        }
        return;
    }

    for change in run.iter().take(before) {
        lines.push(RenderLine::plain(text_of(change)));
    }
    let hidden = run.len().saturating_sub(before).saturating_sub(after);
    // The fold's own line is the first hidden line, and `through` is the last
    // of them, which is what makes expanding it show exactly what was hidden.
    let summary = format!(
        "... {hidden} unchanged line{}",
        if hidden == 1 { "" } else { "s" }
    );
    let first = lines.len();
    for (offset, change) in run.iter().skip(before).take(hidden).enumerate() {
        let mut row = RenderLine::plain(text_of(change));
        if offset == 0 {
            row.spans = whole(&row.text, SynSlot::DiffMarker);
            row.fold = Some(Fold {
                through: first.saturating_add(hidden).saturating_sub(1),
                summary: summary.clone(),
            });
        }
        lines.push(row);
    }
    for change in run.iter().skip(before.saturating_add(hidden)) {
        lines.push(RenderLine::plain(text_of(change)));
    }
}

#[cfg(test)]
#[path = "diff_tests.rs"]
mod tests;
