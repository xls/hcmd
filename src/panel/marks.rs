//! Which rows of a tab are marked, when the answer arrives as a set of names.
//!
//! A tab's marks are a set of [`Entry::mark_key`]s, which is what survives a
//! re-sort: a mark is attached to the row's identity rather than to its
//! position, so `Ctrl+1` does not move the selection to whatever landed on the
//! old row. The `..` row is in no set and is never counted.
//!
//! # Why names come in and keys go out
//!
//! The two comparisons in the design both produce **names**, because that is
//! what comparing two directory listings can produce: the same file on both
//! sides is the same name. Every other mark operation in the crate works in
//! `mark_key`s, because on a flat virtual listing two rows
//! called `mod.rs` from different directories are two different rows. This
//! module is the one place that converts, so both rows called `mod.rs` mark,
//! which is the only answer that survives a re-sort.

use std::collections::HashSet;

use crate::panel::Tab;
use crate::vfs::Entry;

/// Replace a tab's marks with every **shown** row whose **name** is in
/// `names`. Through [`Tab::shown_entries`], like every other mark operation:
/// a mark landing on a row a quick-search filter hides would be one the user
/// cannot see and cannot unmark.
pub fn replace_marks(tab: &mut Tab, names: &HashSet<String>) {
    tab.marks = tab
        .shown_entries()
        .filter(|(_, e)| !e.is_parent && names.contains(&e.name))
        .map(|(_, e)| e.mark_key().into_owned())
        .collect();
}

/// Add to a tab's marks, for the contents job's verdict arriving after the
/// four steps have already marked.
pub fn add_marks(tab: &mut Tab, names: &[String]) {
    let wanted: HashSet<&str> = names.iter().map(String::as_str).collect();
    let more: Vec<String> = tab
        .shown_entries()
        .filter(|(_, e)| !e.is_parent && wanted.contains(e.name.as_str()))
        .map(|(_, e)| e.mark_key().into_owned())
        .collect();
    tab.marks.extend(more);
}

/// How many entries `Shift+F2` looked at, for its status line.
///
/// The distinct names across both sides, `..` excluded: a name on both sides
/// is **one** comparison, so counting the two lists' lengths would report
/// twice the work for two identical directories.
pub fn compared_count(left: &[Entry], right: &[Entry]) -> usize {
    left.iter()
        .chain(right.iter())
        .filter(|e| !e.is_parent)
        .map(|e| e.name.as_str())
        .collect::<HashSet<&str>>()
        .len()
}

/// Sort a tab's entries in place.
///
/// A thin forward to [`Tab::sort_entries`], which is the canonical
/// implementation: the `..` row first, `directories_first` on top of every
/// order, a stable sort, and - the part this used to get wrong - the cursor
/// re-found **by name** afterwards, so `Ctrl+1` twice leaves it on the same
/// entry rather than on whatever landed on its old row.
pub fn sort_entries(tab: &mut Tab, directories_first: bool) {
    tab.sort_entries(directories_first);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::VfsPath;

    fn tab_with(names: &[&str]) -> Tab {
        let mut tab = Tab::new(VfsPath::local_root());
        tab.entries = names.iter().map(|n| Entry::file(*n)).collect();
        tab
    }

    #[test]
    fn a_replaced_set_is_exactly_the_named_rows() {
        let mut tab = tab_with(&["a", "b", "c"]);
        let names: HashSet<String> = ["a", "c"].iter().map(|s| (*s).to_string()).collect();
        replace_marks(&mut tab, &names);
        assert_eq!(tab.marks.len(), 2);
        assert!(tab.marks.contains("a"));
        assert!(tab.marks.contains("c"));

        // Replace, not merge: the previous answer does not survive a second
        // comparison.
        let later: HashSet<String> = ["b"].iter().map(|s| (*s).to_string()).collect();
        replace_marks(&mut tab, &later);
        assert_eq!(tab.marks.len(), 1);
        assert!(tab.marks.contains("b"));
    }

    #[test]
    fn adding_keeps_what_the_four_steps_already_marked() {
        let mut tab = tab_with(&["a", "b", "c"]);
        let first: HashSet<String> = ["a"].iter().map(|s| (*s).to_string()).collect();
        replace_marks(&mut tab, &first);
        add_marks(&mut tab, &["c".to_string()]);
        assert_eq!(tab.marks.len(), 2);
        assert!(tab.marks.contains("a"));
        assert!(tab.marks.contains("c"));
    }

    #[test]
    fn a_name_on_both_sides_is_one_comparison() {
        let left = [Entry::file("a"), Entry::file("b")];
        let right = [Entry::file("b"), Entry::file("c")];
        assert_eq!(compared_count(&left, &right), 3);
    }

    #[test]
    fn a_comparison_marks_only_the_rows_the_filter_shows() {
        // Compare while a quick-search filter is up: a mark on a hidden row
        // would be one the user cannot see, cannot unmark, and would once
        // have fed `F8` an invisible operand.
        let mut tab = tab_with(&["alpha", "beta", "album"]);
        tab.set_quick_filter("al".to_string(), |name: &str| name.starts_with("al"));
        let names: HashSet<String> = ["alpha", "beta"].iter().map(|s| (*s).to_string()).collect();
        replace_marks(&mut tab, &names);
        assert!(tab.marks.contains("alpha"));
        assert!(!tab.marks.contains("beta"), "hidden rows take no marks");

        add_marks(&mut tab, &["beta".to_string(), "album".to_string()]);
        assert!(tab.marks.contains("album"));
        assert!(
            !tab.marks.contains("beta"),
            "not by the late verdict either"
        );
    }

    #[test]
    fn the_parent_row_is_in_no_set_and_is_never_counted() {
        let mut tab = tab_with(&["a"]);
        tab.entries.insert(0, Entry::parent_entry());
        let names: HashSet<String> = ["..", "a"].iter().map(|s| (*s).to_string()).collect();
        replace_marks(&mut tab, &names);
        assert_eq!(tab.marks.len(), 1, "the parent row must not mark");
        add_marks(&mut tab, &["..".to_string()]);
        assert_eq!(tab.marks.len(), 1);
        assert_eq!(compared_count(&tab.entries, &[]), 1);
    }
}
