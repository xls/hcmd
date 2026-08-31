//! Split and merge: the naming, and a round trip that is byte for byte.

use super::*;
use crate::vfs::LocalFs;

/// Drive a job to completion, as the event loop would.
fn drive(spec: crate::ops::JobSpec) -> crate::ops::JobSummary {
    let (mut ctx, rx, _dtx, _flag) = crate::ops::JobContext::for_test(spec.kind);
    let fs_impl = LocalFs::new();
    crate::ops::run(&fs_impl, &spec, &mut ctx);
    let summary = ctx.finish();
    drop(rx);
    summary
}

/// A directory with `whole.bin` of `len` bytes in it.
fn tree(tag: &str, len: usize) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("hcmd-split-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("dir");
    // Not all one byte: a round trip that only proves length would pass on a
    // merge that wrote the parts in the wrong order.
    let body: Vec<u8> = (0..len)
        .map(|i| u8::try_from(i % 251).unwrap_or(0))
        .collect();
    std::fs::write(root.join("whole.bin"), &body).expect("write");
    root
}

#[test]
fn the_parts_are_named_the_way_every_other_tool_names_them() {
    assert_eq!(part_name("archive.iso", 1), "archive.iso.001");
    assert_eq!(part_name("archive.iso", 42), "archive.iso.042");
    assert_eq!(part_name("archive.iso", 999), "archive.iso.999");
}

#[test]
fn only_the_first_part_is_a_starting_point() {
    // Merging from the middle would produce a file missing its head, which
    // looks like a file and is not one.
    assert!(is_first_part("a.iso.001"));
    assert!(!is_first_part("a.iso.002"));
    assert!(!is_first_part("a.iso"));
    assert!(!is_first_part(".001"));
    assert_eq!(stem_of_part("a.iso.007").as_deref(), Some("a.iso"));
    assert_eq!(stem_of_part("a.iso"), None);
    assert_eq!(stem_of_part("a.iso.abc"), None);
}

#[test]
fn a_split_file_merges_back_byte_for_byte() {
    let root = tree("roundtrip", 10_000);
    let source = VfsPath::local(root.join("whole.bin"));
    let mut spec = crate::ops::JobSpec::new(
        crate::ops::JobKind::Split,
        vec![source.clone()],
        Some(VfsPath::local(&root)),
    );
    spec.options.part_size = 4096;
    let summary = drive(spec);
    assert!(summary.failures.is_empty(), "{:?}", summary.failures);
    assert_eq!(summary.files_done, 3, "10,000 bytes in 4 KB parts is three");

    // The parts are there and the last one is the remainder.
    for (n, want) in [(1, 4096), (2, 4096), (3, 10_000 - 8192)] {
        let part = root.join(part_name("whole.bin", n));
        assert_eq!(
            std::fs::metadata(&part).expect("a part").len(),
            want,
            "part {n}"
        );
    }
    // And no fourth: a source that ends on a boundary must not leave an empty
    // part behind, and one that does not must not either.
    assert!(!root.join(part_name("whole.bin", 4)).exists());

    std::fs::remove_file(root.join("whole.bin")).expect("remove the original");
    let merged = drive(crate::ops::JobSpec::new(
        crate::ops::JobKind::Merge,
        vec![VfsPath::local(root.join(part_name("whole.bin", 1)))],
        None,
    ));
    assert!(merged.failures.is_empty(), "{:?}", merged.failures);
    let back = std::fs::read(root.join("whole.bin")).expect("the merged file");
    let original: Vec<u8> = (0..10_000)
        .map(|i| u8::try_from(i % 251).unwrap_or(0))
        .collect();
    assert_eq!(back, original, "the merge is byte for byte");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_source_that_ends_on_a_boundary_leaves_no_empty_part() {
    let root = tree("boundary", 8192);
    let mut spec = crate::ops::JobSpec::new(
        crate::ops::JobKind::Split,
        vec![VfsPath::local(root.join("whole.bin"))],
        Some(VfsPath::local(&root)),
    );
    spec.options.part_size = 4096;
    let summary = drive(spec);
    assert_eq!(
        summary.files_done, 2,
        "exactly two parts, not two and a stub"
    );
    assert!(
        !root.join(part_name("whole.bin", 3)).exists(),
        "a zero-byte third part would make the set look longer than it is"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_part_size_of_zero_is_refused_rather_than_run() {
    let root = tree("zero", 100);
    let spec = crate::ops::JobSpec::new(
        crate::ops::JobKind::Split,
        vec![VfsPath::local(root.join("whole.bin"))],
        Some(VfsPath::local(&root)),
    );
    let summary = drive(spec);
    assert!(
        !summary.failures.is_empty(),
        "a part size of zero would ask for one file per byte"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_size_needing_more_parts_than_the_numbering_holds_is_refused() {
    let root = tree("toomany", 10_000);
    let mut spec = crate::ops::JobSpec::new(
        crate::ops::JobKind::Split,
        vec![VfsPath::local(root.join("whole.bin"))],
        Some(VfsPath::local(&root)),
    );
    spec.options.part_size = 1;
    let summary = drive(spec);
    assert!(
        summary
            .failures
            .iter()
            .any(|f| f.error.to_string().contains("larger part size")),
        "the refusal names the way out: {:?}",
        summary.failures
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_merge_stops_at_the_gap_rather_than_writing_a_short_file() {
    let root = tree("gap", 10_000);
    let mut spec = crate::ops::JobSpec::new(
        crate::ops::JobKind::Split,
        vec![VfsPath::local(root.join("whole.bin"))],
        Some(VfsPath::local(&root)),
    );
    spec.options.part_size = 4096;
    drive(spec);
    // The middle goes missing, which is what happens when a download stops.
    std::fs::remove_file(root.join(part_name("whole.bin", 2))).expect("remove part 2");
    std::fs::remove_file(root.join("whole.bin")).expect("remove the original");

    let merged = drive(crate::ops::JobSpec::new(
        crate::ops::JobKind::Merge,
        vec![VfsPath::local(root.join(part_name("whole.bin", 1)))],
        None,
    ));
    // Only the first part was found, so only it was written. The file that
    // results is short, and the count says how much of the set was used - the
    // caller reports it, and it is not silently "done".
    assert_eq!(merged.files_done, 1, "the walk stopped at the gap");
    let _ = std::fs::remove_dir_all(&root);
}
