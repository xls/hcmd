use super::*;

#[test]
fn append_resumes_and_every_overwrite_fetches_afresh() {
    let path = Path::new("/dl/file.zip");
    assert_eq!(
        outcome_for(path, ConflictChoice::Append, None),
        Outcome::Fetch {
            path: path.to_path_buf(),
            resume: true
        }
    );
    // A download has no source date or size to compare until it is fetched,
    // so the conditional overwrites can only mean "overwrite".
    for choice in [
        ConflictChoice::Overwrite,
        ConflictChoice::OverwriteIfNewer,
        ConflictChoice::OverwriteIfDifferentSize,
    ] {
        assert_eq!(
            outcome_for(path, choice, None),
            Outcome::Fetch {
                path: path.to_path_buf(),
                resume: false
            },
            "{choice:?}"
        );
    }
    assert_eq!(outcome_for(path, ConflictChoice::Skip, None), Outcome::Skip);
}

#[test]
fn rename_takes_the_typed_name_beside_the_file_or_a_free_one() {
    let dir = std::env::temp_dir().join(format!("hcmd-dl-rename-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("file.zip");
    std::fs::write(&path, b"x").expect("the file in the way");

    // Typed: exactly that, next to the file.
    assert_eq!(
        outcome_for(&path, ConflictChoice::Rename, Some("other.zip")),
        Outcome::Fetch {
            path: dir.join("other.zip"),
            resume: false
        }
    );
    // Blank: a free numbered name, the extension kept.
    assert_eq!(
        outcome_for(&path, ConflictChoice::Rename, Some("  ")),
        Outcome::Fetch {
            path: dir.join("file (2).zip"),
            resume: false
        }
    );
    assert_eq!(
        outcome_for(&path, ConflictChoice::Rename, None),
        Outcome::Fetch {
            path: dir.join("file (2).zip"),
            resume: false
        }
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_free_name_is_used_as_is_and_a_taken_one_is_numbered() {
    let dir = std::env::temp_dir().join(format!("hcmd-dl-free-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");

    // Nothing there yet: the plain name.
    assert_eq!(free_name(&dir, "file.zip"), dir.join("file.zip"));
    // Once it exists, the next one is numbered, keeping the extension.
    std::fs::write(dir.join("file.zip"), b"x").expect("write");
    assert_eq!(free_name(&dir, "file.zip"), dir.join("file (2).zip"));
    // A name with no extension is numbered without inventing one.
    std::fs::write(dir.join("data"), b"x").expect("write");
    assert_eq!(free_name(&dir, "data"), dir.join("data (2)"));

    let _ = std::fs::remove_dir_all(&dir);
}
