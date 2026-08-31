//! Checksums: the formats, and what a sidecar is allowed to name.

use super::*;
use crate::vfs::VfsPath;

#[test]
fn a_sha256_sidecar_is_what_sha256sum_writes() {
    // Two spaces, hash first. One space means "text mode" to some
    // implementations, and is the difference between a file that verifies
    // with `sha256sum -c` and one that does not.
    let entries = vec![Entry {
        name: "notes.txt".to_string(),
        digest: "abc123".to_string(),
    }];
    assert_eq!(
        write_sidecar(&entries, Digest::Sha256),
        "abc123  notes.txt\n"
    );
}

#[test]
fn an_sfv_sidecar_puts_the_name_first() {
    let entries = vec![Entry {
        name: "notes.txt".to_string(),
        digest: "deadbeef".to_string(),
    }];
    assert_eq!(
        write_sidecar(&entries, Digest::Crc32),
        "notes.txt deadbeef\n"
    );
}

#[test]
fn a_sidecar_round_trips() {
    let entries = vec![
        Entry {
            name: "one.txt".to_string(),
            digest: "aa".to_string(),
        },
        Entry {
            name: "two files.bin".to_string(),
            digest: "bb".to_string(),
        },
    ];
    for digest in [Digest::Sha256, Digest::Crc32] {
        let text = write_sidecar(&entries, digest);
        assert_eq!(read_sidecar(&text, digest), entries, "{digest:?}");
    }
}

#[test]
fn a_name_with_spaces_survives_both_formats() {
    // The name is everything after the first field in a `.sha256` and
    // everything before the last in a `.sfv`, which is what lets a file called
    // `two files.bin` be checksummed at all.
    let sha = read_sidecar("aa  two files.bin\n", Digest::Sha256);
    assert_eq!(sha[0].name, "two files.bin");
    let sfv = read_sidecar("two files.bin bb\n", Digest::Crc32);
    assert_eq!(sfv[0].name, "two files.bin");
}

#[test]
fn comments_and_blank_lines_are_skipped() {
    let text = "; made by something else\n\naa  one.txt\n# another comment\n";
    let entries = read_sidecar(text, Digest::Sha256);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "one.txt");
}

#[test]
fn a_binary_mode_star_is_not_part_of_the_name() {
    // `sha256sum -b` writes `<hash> *<name>`. The star is a mode marker and
    // not a character in the filename.
    let entries = read_sidecar("aa  *image.png\n", Digest::Sha256);
    assert_eq!(entries[0].name, "image.png");
}

#[test]
fn a_line_that_cannot_be_read_does_not_lose_the_rest() {
    let entries = read_sidecar("nonsense\naa  good.txt\n", Digest::Sha256);
    assert_eq!(entries.len(), 1, "the good line survives a bad one");
    assert_eq!(entries[0].name, "good.txt");
}

#[test]
fn a_sidecar_may_not_name_a_file_outside_its_own_directory() {
    // A sidecar is data from wherever it was downloaded from. A line naming
    // `../../etc/passwd` would otherwise be an instruction to read that file
    // and report on it.
    for bad in [
        "../secrets",
        "../../etc/passwd",
        "/etc/passwd",
        "sub/../../out",
        "\\\\server\\share",
        "C:\\Windows\\notepad.exe",
        "./here",
        "",
    ] {
        assert!(!name_is_safe(bad), "{bad} should be refused");
    }
    for good in ["notes.txt", "sub/notes.txt", "a b/c d.bin"] {
        assert!(name_is_safe(good), "{good} should be allowed");
    }
}

#[test]
fn the_digest_is_recognised_from_the_sidecars_name() {
    assert_eq!(Digest::of_name("sums.sha256"), Some(Digest::Sha256));
    assert_eq!(Digest::of_name("SUMS.SHA256"), Some(Digest::Sha256));
    assert_eq!(Digest::of_name("disc.sfv"), Some(Digest::Crc32));
    assert_eq!(Digest::of_name("notes.txt"), None);
}

/// A directory with two files, and a `sums.sha256` written by `sha256sum`.
///
/// `None` when `sha256sum` is not installed, which the caller reports as a
/// skip: the point of this test is interoperability, and there is nothing to
/// interoperate with.
fn tree_with_real_sums(tag: &str) -> Option<std::path::PathBuf> {
    let root = std::env::temp_dir().join(format!("hcmd-sumop-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).ok()?;
    std::fs::write(root.join("alpha.txt"), b"alpha contents\n").ok()?;
    std::fs::write(root.join("beta.txt"), b"beta contents\n").ok()?;
    let made = std::process::Command::new("sh")
        .arg("-c")
        .arg("sha256sum alpha.txt beta.txt > sums.sha256")
        .current_dir(&root)
        .output()
        .ok()?;
    made.status.success().then_some(root)
}

/// Drive a checksum job to completion, as the event loop would.
fn drive(spec: crate::ops::JobSpec) -> crate::ops::JobSummary {
    let (mut ctx, rx, _dtx, _flag) = crate::ops::JobContext::for_test(spec.kind);
    let fs_impl = crate::vfs::LocalFs::new();
    crate::ops::run(&fs_impl, &spec, &mut ctx);
    let summary = ctx.finish();
    drop(rx);
    summary
}

#[test]
fn a_file_sha256sum_wrote_verifies_here() {
    // The half that matters when the `.sha256` arrived with a download.
    let Some(root) = tree_with_real_sums("good") else {
        eprintln!("SKIPPING a_file_sha256sum_wrote_verifies_here: sha256sum is not installed");
        return;
    };
    let summary = drive(crate::ops::JobSpec::new(
        crate::ops::JobKind::Checksum { verify: true },
        vec![VfsPath::local(root.join("sums.sha256"))],
        None,
    ));
    let _ = std::fs::remove_dir_all(&root);
    assert!(summary.failures.is_empty(), "{:?}", summary.failures);
    assert_eq!(summary.files_done, 2, "both files were checked");
    assert!(
        summary.differing.is_empty(),
        "two matching files were reported as differing: {:?}",
        summary.differing
    );
}

#[test]
fn a_file_that_changed_since_is_named() {
    let Some(root) = tree_with_real_sums("changed") else {
        eprintln!("SKIPPING a_file_that_changed_since_is_named: sha256sum is not installed");
        return;
    };
    // Edited after the list was made, which is the whole point of having one.
    std::fs::write(root.join("beta.txt"), b"tampered\n").expect("rewrite");
    let summary = drive(crate::ops::JobSpec::new(
        crate::ops::JobKind::Checksum { verify: true },
        vec![VfsPath::local(root.join("sums.sha256"))],
        None,
    ));
    let _ = std::fs::remove_dir_all(&root);
    assert!(summary.failures.is_empty(), "{:?}", summary.failures);
    assert_eq!(
        summary.differing,
        vec!["beta.txt".to_string()],
        "the changed file is named, and only it"
    );
}

#[test]
fn a_line_naming_a_file_outside_the_directory_is_refused_not_read() {
    // A checksum file is data from wherever it came from. This one is hostile.
    let root = std::env::temp_dir().join(format!("hcmd-sumesc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("dir");
    std::fs::write(
        root.join("evil.sha256"),
        "0000000000000000000000000000000000000000000000000000000000000000  ../../etc/passwd\n",
    )
    .expect("write");
    let summary = drive(crate::ops::JobSpec::new(
        crate::ops::JobKind::Checksum { verify: true },
        vec![VfsPath::local(root.join("evil.sha256"))],
        None,
    ));
    let _ = std::fs::remove_dir_all(&root);
    assert!(
        summary.differing.iter().any(|d| d.contains("refused")),
        "a name outside the directory was followed rather than refused: {:?}",
        summary.differing
    );
}
