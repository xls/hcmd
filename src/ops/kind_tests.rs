//! The kinds: which change the filesystem and which do not.

use super::*;

#[test]
fn a_size_walk_and_a_compare_are_the_only_kinds_that_change_nothing() {
    assert!(!JobKind::Size.is_destructive());
    assert!(
        !JobKind::Compare.is_destructive(),
        "the contents comparison reads both sides and writes nothing"
    );
    for kind in [
        JobKind::Copy,
        JobKind::Move,
        JobKind::Mkdir,
        JobKind::Delete { trash: true },
        JobKind::Delete { trash: false },
    ] {
        assert!(kind.is_destructive(), "{kind} should be destructive");
    }
}
