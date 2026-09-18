use super::*;

fn two_roots() -> Vec<Root> {
    roots(&[
        PathBuf::from("/srv/photos"),
        PathBuf::from("/home/t/report.pdf"),
        PathBuf::from("/other/report.pdf"),
    ])
}

#[test]
fn roots_are_named_by_file_name_and_numbered_when_they_collide() {
    let r = two_roots();
    let names: Vec<&str> = r.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["photos", "report.pdf", "report.pdf (2)"]);
    // A filesystem root has no name to serve under and is left out.
    assert!(roots(&[PathBuf::from("/")]).is_empty());
}

#[test]
fn the_share_root_lists_and_a_root_name_resolves_under_its_real_path() {
    let r = two_roots();
    assert_eq!(resolve(&r, "/"), Resolved::Index);
    assert_eq!(resolve(&r, ""), Resolved::Index);
    assert_eq!(
        resolve(&r, "/photos/2026/summer/a b.jpg"),
        Resolved::Path {
            root: "photos".into(),
            path: PathBuf::from("/srv/photos/2026/summer/a b.jpg"),
            url: "/photos/2026/summer/a%20b.jpg".into(),
        }
    );
    // A trailing slash is kept on the URL, so a folder's links are relative
    // to itself.
    assert_eq!(
        resolve(&r, "/photos/2026/"),
        Resolved::Path {
            root: "photos".into(),
            path: PathBuf::from("/srv/photos/2026"),
            url: "/photos/2026/".into(),
        }
    );
    // The numbered twin is its own root.
    assert!(matches!(
        resolve(&r, "/report.pdf (2)"),
        Resolved::Path { path, .. } if path.as_path() == Path::new("/other/report.pdf")
    ));
    assert_eq!(resolve(&r, "/nothing/here"), Resolved::NotFound);
}

#[test]
fn a_path_that_could_leave_the_share_is_refused_before_the_disk_is_touched() {
    let r = two_roots();
    for bad in [
        "/photos/../../etc/passwd",
        "/photos/..",
        "/../photos",
        "/photos//x",
        "/photos/./x",
        "/photos/a\\b",
    ] {
        assert_eq!(resolve(&r, bad), Resolved::Forbidden, "{bad}");
    }
}

#[test]
fn a_symlink_out_of_the_share_does_not_stay_inside() {
    let dir = std::env::temp_dir().join(format!("hcmd-serve-tree-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("share")).expect("dirs");
    std::fs::write(dir.join("share/in.txt"), b"x").expect("file");
    std::fs::write(dir.join("outside.txt"), b"y").expect("file");
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.join("outside.txt"), dir.join("share/leak")).expect("link");

    let root = dir.join("share");
    assert!(stays_inside(&root, &root.join("in.txt")));
    #[cfg(unix)]
    assert!(
        !stays_inside(&root, &root.join("leak")),
        "a symlink out is out"
    );
    assert!(
        !stays_inside(&root, &root.join("missing")),
        "nothing there is not inside"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
