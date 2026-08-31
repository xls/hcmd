//! Reading a blob out of a real repository, loose and packed.

use super::*;

/// A repository with one commit, built with `git` itself.
///
/// Running `git` in a **test** is not the rule this program follows at
/// runtime: the rule is that the program starts no subprocess for its own
/// work, and building a fixture is not the program's work. Reading it back is,
/// and that is what is under test.
struct Repo {
    root: std::path::PathBuf,
}

impl Repo {
    fn new(tag: &str) -> Option<Self> {
        let root = std::env::temp_dir().join(format!("hcmd-git-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).ok()?;
        let repo = Self { root };
        repo.git(&["init", "-q"])?;
        repo.git(&["config", "user.email", "t@example.invalid"])?;
        repo.git(&["config", "user.name", "Test"])?;
        Some(repo)
    }

    fn git(&self, args: &[&str]) -> Option<String> {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn write(&self, name: &str, body: &str) {
        std::fs::write(self.root.join(name), body).expect("write");
    }

    fn commit(&self, message: &str) -> Option<()> {
        self.git(&["add", "-A"])?;
        self.git(&["commit", "-q", "-m", message])?;
        Some(())
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Skips rather than fails where `git` is not installed, and says so.
macro_rules! repo_or_skip {
    ($tag:expr) => {
        match Repo::new($tag) {
            Some(repo) => repo,
            None => {
                eprintln!("SKIPPING {}: git is not installed", $tag);
                return;
            }
        }
    };
}

#[test]
fn it_reads_the_committed_version_of_a_modified_file() {
    let repo = repo_or_skip!("modified");
    repo.write("notes.txt", "committed\n");
    if repo.commit("one").is_none() {
        eprintln!("SKIPPING modified: git could not commit");
        return;
    }
    // The working copy moves on; HEAD does not.
    repo.write("notes.txt", "working\n");

    let found = head_blob(&repo.root.join("notes.txt"))
        .expect("a readable repository")
        .expect("a tracked file");
    assert_eq!(String::from_utf8_lossy(&found.bytes), "committed\n");
    assert_eq!(found.label, "notes.txt (HEAD)");
}

#[test]
fn it_reads_a_blob_out_of_a_packfile() {
    // The case that decided the implementation. A file committed at any point
    // in the past is almost always packed, behind an index and a chain of
    // deltas, and a reader that only handled loose objects would work on a
    // fresh commit and fail on every real repository.
    let repo = repo_or_skip!("packed");
    repo.write("notes.txt", "committed\n");
    if repo.commit("one").is_none() {
        eprintln!("SKIPPING packed: git could not commit");
        return;
    }
    // Force every loose object into a pack, and prove it happened before
    // asserting anything about reading one.
    if repo
        .git(&["gc", "--aggressive", "--prune=now", "-q"])
        .is_none()
    {
        eprintln!("SKIPPING packed: git gc failed");
        return;
    }
    let packs = std::fs::read_dir(repo.root.join(".git/objects/pack"))
        .expect("a pack directory")
        .filter_map(std::result::Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
        .count();
    assert!(packs > 0, "the fixture did not actually pack anything");

    let found = head_blob(&repo.root.join("notes.txt"))
        .expect("a readable repository")
        .expect("a tracked file");
    assert_eq!(String::from_utf8_lossy(&found.bytes), "committed\n");
}

#[test]
fn a_file_that_is_not_tracked_is_not_an_error() {
    let repo = repo_or_skip!("untracked");
    repo.write("committed.txt", "a\n");
    if repo.commit("one").is_none() {
        eprintln!("SKIPPING untracked: git could not commit");
        return;
    }
    repo.write("fresh.txt", "never committed\n");
    assert_eq!(
        head_blob(&repo.root.join("fresh.txt")).expect("a readable repository"),
        None,
        "untracked is a thing to say, not a failure to report"
    );
}

#[test]
fn a_repository_with_no_commits_is_not_an_error() {
    let repo = repo_or_skip!("nocommits");
    repo.write("fresh.txt", "a\n");
    assert_eq!(
        head_blob(&repo.root.join("fresh.txt")).expect("a readable repository"),
        None
    );
}

#[test]
fn a_file_outside_any_repository_is_not_an_error() {
    let dir = std::env::temp_dir().join(format!("hcmd-nogit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join("lonely.txt"), "a\n").expect("write");
    let answer = head_blob(&dir.join("lonely.txt")).expect("no repository is not an error");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(answer, None);
}

#[test]
fn history_lists_commits_newest_first() {
    let repo = repo_or_skip!("history");
    repo.write("a.txt", "one\n");
    if repo.commit("first").is_none() {
        eprintln!("SKIPPING history: commit failed");
        return;
    }
    repo.write("b.txt", "two\n");
    repo.commit("second").expect("second commit");

    let commits = history(&repo.root).expect("history");
    assert_eq!(commits.len(), 2, "two commits");
    assert_eq!(commits[0].subject, "second", "newest first");
    assert_eq!(commits[1].subject, "first");
    assert_eq!(commits[0].short.len(), 8);
    assert!(commits[0].time.is_some());
}

#[test]
fn changed_lists_what_a_commit_touched() {
    let repo = repo_or_skip!("changed");
    repo.write("keep.txt", "unchanged\n");
    repo.write("edit.txt", "before\n");
    if repo.commit("base").is_none() {
        eprintln!("SKIPPING changed: commit failed");
        return;
    }
    // Second commit changes one file and adds another; keep.txt is untouched.
    repo.write("edit.txt", "after\n");
    repo.write("new.txt", "fresh\n");
    repo.commit("change").expect("commit");

    let commits = history(&repo.root).expect("history");
    let touched = changed(&repo.root, &commits[0].id).expect("changed");
    let names: Vec<&str> = touched.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(
        names,
        vec!["edit.txt", "new.txt"],
        "only what changed, sorted"
    );
    assert!(
        !names.contains(&"keep.txt"),
        "an untouched file is not listed"
    );
}

#[test]
fn the_root_commit_lists_everything_it_created() {
    let repo = repo_or_skip!("root");
    repo.write("a.txt", "a\n");
    repo.write("b.txt", "b\n");
    if repo.commit("root").is_none() {
        eprintln!("SKIPPING root: commit failed");
        return;
    }
    let commits = history(&repo.root).expect("history");
    let touched = changed(&repo.root, &commits[0].id).expect("changed");
    let names: Vec<&str> = touched.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(
        names,
        vec!["a.txt", "b.txt"],
        "a root commit created every file"
    );
}

#[test]
fn file_at_reads_a_file_as_of_a_commit() {
    let repo = repo_or_skip!("fileat");
    repo.write("notes.txt", "first version\n");
    if repo.commit("v1").is_none() {
        eprintln!("SKIPPING file_at: commit failed");
        return;
    }
    repo.write("notes.txt", "second version\n");
    repo.commit("v2").expect("commit");
    let commits = history(&repo.root).expect("history");

    // The older commit's version, not the working copy's.
    let old = file_at(&repo.root, &commits[1].id, "notes.txt").expect("file at v1");
    assert_eq!(String::from_utf8_lossy(&old), "first version\n");
    let new = file_at(&repo.root, &commits[0].id, "notes.txt").expect("file at v2");
    assert_eq!(String::from_utf8_lossy(&new), "second version\n");

    // A path that is not in that commit is an error, not an empty file.
    assert!(file_at(&repo.root, &commits[0].id, "absent.txt").is_err());
}

#[test]
fn a_directory_outside_a_repository_has_no_history() {
    let dir = std::env::temp_dir().join(format!("hcmd-nogit-hist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let out = history(&dir).expect("no repository is not an error");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(out.is_empty());
}
