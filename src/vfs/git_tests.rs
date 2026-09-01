//! GitFs: the three levels, browsed as a tree.

use super::*;

/// A repository with two commits, built with `git`.
struct Repo {
    root: std::path::PathBuf,
}

impl Repo {
    fn new(tag: &str) -> Option<Self> {
        let root = std::env::temp_dir().join(format!("hcmd-gitfs-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).ok()?;
        let repo = Self { root };
        repo.git(&["init", "-q"])?;
        repo.git(&["config", "user.email", "t@example.invalid"])?;
        repo.git(&["config", "user.name", "Test"])?;
        repo.write("notes.txt", "first\n");
        repo.commit("initial")?;
        repo.write("notes.txt", "second\n");
        repo.write("added.txt", "new\n");
        repo.commit("a change")?;
        Some(repo)
    }

    fn git(&self, args: &[&str]) -> Option<()> {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|_| ())
    }

    fn write(&self, name: &str, body: &str) {
        std::fs::write(self.root.join(name), body).expect("write");
    }

    fn commit(&self, message: &str) -> Option<()> {
        self.git(&["add", "-A"])?;
        self.git(&["commit", "-q", "-m", message])
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

macro_rules! repo_or_skip {
    ($tag:expr) => {
        match Repo::new($tag) {
            Some(r) => r,
            None => {
                eprintln!("SKIPPING {}: git is not installed", $tag);
                return;
            }
        }
    };
}

/// Drain a `read_dir` receiver to its content rows, dropping the leading `..`
/// (asserted on its own in `a_git_listing_leads_with_the_parent_row`).
async fn rows(fs: &GitFs, path: &VfsPath) -> Vec<Entry> {
    let mut rx = fs.read_dir(path);
    let mut out = Vec::new();
    while let Some(item) = rx.recv().await {
        if let Ok(entry) = item
            && !entry.is_parent
        {
            out.push(entry);
        }
    }
    out
}

#[tokio::test]
async fn a_git_listing_leads_with_the_parent_row() {
    // The revision browser used to be the one directory-shaped backend with no
    // `..` row; now it gives one at every level, so a subfolder inside a commit
    // has a visible way back up.
    let repo = repo_or_skip!("parentrow");
    let base = VfsPath::local(&repo.root).with_segment(BackendKind::Git, "/");
    let fs = GitFs::open(base.clone()).expect("open");

    let mut rx = fs.read_dir(&base);
    let first = rx.recv().await.expect("a row").expect("not an error");
    assert!(
        first.is_parent,
        "the `..` row comes first at the commit list"
    );
    assert_eq!(first.name, "..");

    // And inside a commit, too.
    let commits = rows(&fs, &base).await;
    let sha = commits[0].name.split_whitespace().next().expect("a sha");
    let commit_path = VfsPath::local(&repo.root).with_segment(BackendKind::Git, format!("/{sha}"));
    let mut rx = fs.read_dir(&commit_path);
    let first = rx.recv().await.expect("a row").expect("not an error");
    assert!(first.is_parent, "and inside a revision");
}

#[tokio::test]
async fn the_root_lists_the_commits_newest_first() {
    let repo = repo_or_skip!("commits");
    let display = VfsPath::local(&repo.root).with_segment(BackendKind::Git, "/");
    let fs = GitFs::open(display.clone()).expect("open");
    let commits = rows(&fs, &display).await;
    assert_eq!(commits.len(), 2, "two commits");
    assert!(commits.iter().all(Entry::is_dir), "a commit is a directory");
    assert!(
        commits[0].name.contains("a change"),
        "the newest first, subject in the row: {}",
        commits[0].name
    );
    assert!(commits[1].name.contains("initial"), "{}", commits[1].name);
}

#[tokio::test]
async fn a_commit_lists_the_files_it_changed() {
    let repo = repo_or_skip!("changed");
    let base = VfsPath::local(&repo.root).with_segment(BackendKind::Git, "/");
    let fs = GitFs::open(base.clone()).expect("open");
    let commits = rows(&fs, &base).await;
    // The newest commit's short id is the head of its row name.
    let sha = commits[0].name.split_whitespace().next().expect("a sha");
    let commit_path = VfsPath::local(&repo.root).with_segment(BackendKind::Git, format!("/{sha}"));
    let changed = rows(&fs, &commit_path).await;
    let names: Vec<&str> = changed.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["added.txt", "notes.txt"],
        "only what it changed"
    );
    assert!(
        changed.iter().all(|e| !e.is_dir()),
        "changed files are files"
    );
}

#[tokio::test]
async fn a_file_reads_as_of_its_commit() {
    let repo = repo_or_skip!("fileat");
    let base = VfsPath::local(&repo.root).with_segment(BackendKind::Git, "/");
    let fs = GitFs::open(base.clone()).expect("open");
    let commits = rows(&fs, &base).await;
    // The *older* commit, whose notes.txt says "first".
    let old_sha = commits[1].name.split_whitespace().next().expect("a sha");
    let file =
        VfsPath::local(&repo.root).with_segment(BackendKind::Git, format!("/{old_sha}/notes.txt"));

    use std::io::Read as _;
    let mut reader = fs.open_read(&file).expect("open_read");
    let mut body = String::new();
    reader.read_to_string(&mut body).expect("read");
    assert_eq!(body, "first\n", "the file as it was at that commit");

    let stat = fs.stat(&file).expect("stat");
    assert_eq!(stat.name, "notes.txt");
    assert_eq!(stat.size, "first\n".len() as u64);
}

#[test]
fn a_directory_not_in_a_repository_is_refused_with_a_reason() {
    let dir = std::env::temp_dir().join(format!("hcmd-gitfs-none-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let display = VfsPath::local(&dir).with_segment(BackendKind::Git, "/");
    let err = GitFs::open(display).expect_err("no repository");
    assert!(err.to_string().contains("not a git repository"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn history_is_read_only() {
    let repo = repo_or_skip!("readonly");
    let display = VfsPath::local(&repo.root).with_segment(BackendKind::Git, "/");
    let fs = GitFs::open(display.clone()).expect("open");
    assert!(!fs.capabilities().writable, "history cannot be written");
    let file = VfsPath::local(&repo.root).with_segment(BackendKind::Git, "/deadbeef/x");
    assert!(fs.open_write(&file).is_err(), "no writing a commit");
    assert!(fs.remove(&file).is_err());
    assert!(fs.create_dir(&file).is_err());
}
