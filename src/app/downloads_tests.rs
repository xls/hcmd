use super::*;

#[test]
fn a_name_is_the_last_path_segment_without_the_query() {
    assert_eq!(download_name("https://host/dir/file.zip"), "file.zip");
    assert_eq!(
        download_name("https://host/file.tar.gz?token=abc"),
        "file.tar.gz"
    );
    assert_eq!(download_name("https://host/file.iso#frag"), "file.iso");
    // A trailing slash is not the name.
    assert_eq!(download_name("https://host/dir/"), "dir");
}

#[test]
fn a_url_with_no_usable_name_falls_back() {
    assert_eq!(download_name("https://host"), "download");
    assert_eq!(download_name("https://host/"), "download");
    assert_eq!(download_name("https://host/?q=1"), "download");
}

#[test]
fn a_name_that_would_escape_the_folder_is_refused() {
    // These come off the network and are about to be joined onto the downloads
    // folder, so their shape is refused rather than followed.
    assert_eq!(download_name("https://host/a/../../etc/passwd"), "passwd");
    // A bare `..` segment is not a name.
    assert_eq!(download_name("https://host/.."), "download");
}

#[test]
fn a_free_name_is_used_as_is_and_a_taken_one_is_numbered() {
    let dir = std::env::temp_dir().join(format!("hcmd-dl-name-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");

    // Nothing there yet: the plain name.
    assert_eq!(unique_name(&dir, "file.zip"), dir.join("file.zip"));

    // Once it exists, the next one is numbered, keeping the extension.
    std::fs::write(dir.join("file.zip"), b"x").expect("write");
    assert_eq!(unique_name(&dir, "file.zip"), dir.join("file (2).zip"));

    // A name with no extension is numbered without inventing one.
    std::fs::write(dir.join("data"), b"x").expect("write");
    assert_eq!(unique_name(&dir, "data"), dir.join("data (2)"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_folder_is_made_on_first_use_and_removed_on_drop() {
    let root;
    {
        let mut downloads = Downloads::default();
        assert!(
            downloads.root().is_none(),
            "nothing until the first download"
        );
        let dir = downloads.dir().expect("the folder is made").to_path_buf();
        assert!(dir.is_dir());
        assert!(downloads.root().is_some());
        root = dir;
    }
    // Dropped: the staging area is gone.
    assert!(!root.exists(), "the downloads folder is emptied on drop");
}
