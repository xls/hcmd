//! Links and permissions, against a real filesystem.

use super::*;
use crate::vfs::LocalFs;

/// A directory with one file in it.
fn tree(tag: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("hcmd-link-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("dir");
    std::fs::write(root.join("target.txt"), b"contents\n").expect("write");
    root
}

#[test]
fn a_symbolic_link_points_where_it_was_told_and_is_not_resolved() {
    let root = tree("symlink");
    let fs_impl = LocalFs::new();
    let outcome = make_link(
        &fs_impl,
        &LinkRequest {
            target: VfsPath::local(root.join("target.txt")),
            link: VfsPath::local(root.join("link.txt")),
            symbolic: true,
        },
    );
    assert!(outcome.message.contains("created"), "{}", outcome.message);
    assert_eq!(outcome.select.as_deref(), Some("link.txt"));
    let read = std::fs::read_link(root.join("link.txt")).expect("a symbolic link");
    assert_eq!(read, root.join("target.txt"));
    // And it reads through to the file.
    assert_eq!(
        std::fs::read(root.join("link.txt")).expect("read through"),
        b"contents\n"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_symbolic_link_may_point_at_something_that_is_not_there() {
    // The property that makes symbolic links useful, and the reason the target
    // is written as text rather than resolved.
    let root = tree("dangling");
    let fs_impl = LocalFs::new();
    let outcome = make_link(
        &fs_impl,
        &LinkRequest {
            target: VfsPath::local(root.join("not-yet.txt")),
            link: VfsPath::local(root.join("link.txt")),
            symbolic: true,
        },
    );
    assert!(outcome.message.contains("created"), "{}", outcome.message);
    assert!(
        std::fs::symlink_metadata(root.join("link.txt")).is_ok(),
        "the link exists even though its target does not"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_hard_link_is_a_second_name_for_one_file() {
    let root = tree("hardlink");
    let fs_impl = LocalFs::new();
    let outcome = make_link(
        &fs_impl,
        &LinkRequest {
            target: VfsPath::local(root.join("target.txt")),
            link: VfsPath::local(root.join("second.txt")),
            symbolic: false,
        },
    );
    assert!(outcome.message.contains("created"), "{}", outcome.message);
    // Writing through one name is visible through the other, which is the
    // whole difference from a copy.
    std::fs::write(root.join("second.txt"), b"changed\n").expect("write");
    assert_eq!(
        std::fs::read(root.join("target.txt")).expect("read"),
        b"changed\n"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_link_that_cannot_be_made_says_so_rather_than_claiming_it_did() {
    let root = tree("refused");
    let fs_impl = LocalFs::new();
    // A hard link into a directory that does not exist.
    let outcome = make_link(
        &fs_impl,
        &LinkRequest {
            target: VfsPath::local(root.join("target.txt")),
            link: VfsPath::local(root.join("nowhere/second.txt")),
            symbolic: false,
        },
    );
    assert!(!outcome.message.contains("created"), "{}", outcome.message);
    assert_eq!(outcome.select, None, "nothing to put the cursor on");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn permissions_are_set_and_counted() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tree("chmod");
    std::fs::write(root.join("second.txt"), b"x").expect("write");
    let fs_impl = LocalFs::new();
    let outcome = set_modes(
        &fs_impl,
        &ChmodRequest {
            paths: vec![
                VfsPath::local(root.join("target.txt")),
                VfsPath::local(root.join("second.txt")),
            ],
            mode: 0o600,
        },
    );
    assert!(
        outcome.message.starts_with("2 set to 600"),
        "{}",
        outcome.message
    );
    for name in ["target.txt", "second.txt"] {
        let mode = std::fs::metadata(root.join(name))
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "{name}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn what_refused_is_named_and_the_rest_still_happened() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tree("partial");
    let fs_impl = LocalFs::new();
    let outcome = set_modes(
        &fs_impl,
        &ChmodRequest {
            paths: vec![
                VfsPath::local(root.join("target.txt")),
                VfsPath::local(root.join("missing.txt")),
            ],
            mode: 0o640,
        },
    );
    assert!(
        outcome.message.contains("1 set to 640"),
        "{}",
        outcome.message
    );
    assert!(
        outcome.message.contains("missing.txt"),
        "the refusal names what refused: {}",
        outcome.message
    );
    // And the one that could be changed was.
    let mode = std::fs::metadata(root.join("target.txt"))
        .expect("stat")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o640);
    let _ = std::fs::remove_dir_all(&root);
}
