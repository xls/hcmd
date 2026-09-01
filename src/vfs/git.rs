//! Browsing a repository's history as a directory tree.
//!
//! A [`Vfs`] over the git object store, addressed by a [`BackendKind::Git`]
//! segment the way an archive or a disk image is. Three levels, and no more:
//!
//! * `repo#git/` lists the commits, newest first, each a directory named by
//!   its short id.
//! * `repo#git/<sha>/` lists the files that commit changed, against its first
//!   parent. A root commit lists everything it created.
//! * `repo#git/<sha>/<path>` is that file, **as of that commit** - a real,
//!   readable path, so `F3` views it, `Alt+D` diffs it, and `F5` copies it out
//!   into a panel.
//!
//! It is why this is a `Vfs` and not a viewer: everything the panel already
//! does works on these paths for free, because they are paths. Nothing new had
//! to be taught about selection, navigation, or opening a file.
//!
//! # Read only, and not as a step towards writing
//!
//! History is not something a file manager should be able to edit by copying
//! onto it. Every write refuses, and the capability says so before a copy into
//! here is offered, the same way an archive member and a disk image already
//! do.

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::error::{Error, Result};
use crate::vfs::{
    BackendKind, Capabilities, Entry, EntryKind, LatencyClass, READ_DIR_CHANNEL_DEPTH, ReadSeek,
    Vfs, VfsPath,
};

/// One open history browser, rooted at a repository's working directory.
#[derive(Debug, Clone)]
pub struct GitFs {
    /// The directory the `#git/` segment was opened from, which is what git is
    /// discovered from. Held so every read finds the same repository.
    dir: PathBuf,
}

/// How a `#git/` segment's tail splits into a commit and a path within it.
enum Location {
    /// `#git/` itself: the list of commits.
    Commits,
    /// `#git/<sha>/<path>`: a path within a commit's tree. `path` is empty for
    /// the commit's root. Whether it is a directory to list or a file to read
    /// is decided by resolving it, not by its name.
    In { sha: String, path: String },
}

impl GitFs {
    /// Open the history of the repository containing `display`'s local path.
    ///
    /// Answers an error when there is no repository, rather than an empty
    /// listing: a panel opened onto `#git/` where there is no git is a mistake
    /// the caller should hear about, unlike a directory that merely happens
    /// not to be tracked.
    pub fn open(display: VfsPath) -> Result<Self> {
        // The directory the `#git` segment hangs off, which is the outermost
        // segment and must be a local one: a repository is on the disk. Read
        // from the head rather than `local_path`, which answers only for a
        // path of one segment and a git path has at least two.
        let dir = match display.segments().first() {
            Some((BackendKind::Local, path)) => path.clone(),
            _ => {
                return Err(Error::msg("git history is only for a local directory"));
            }
        };
        if crate::git::history(&dir)?.is_empty() && !dir.join(".git").exists() {
            // `history` answers empty both for "no repository" and "no commits
            // yet"; the `.git` probe separates them so the message is right.
            return Err(Error::msg(format!(
                "{}: not a git repository",
                dir.display()
            )));
        }
        // `display` is consumed only to find the repository directory above;
        // the panel title is derived from the live path when it is drawn.
        let _ = display;
        Ok(Self { dir })
    }

    /// Split a path's `#git` tail into a commit and a path within it.
    fn locate(tail: &std::path::Path) -> Location {
        let text = tail.to_string_lossy();
        let trimmed = text.trim_matches('/');
        if trimmed.is_empty() {
            return Location::Commits;
        }
        // The commit id is the head of the first component, before any space:
        // a commit row's name is `<short>  <subject>`, and only the short id
        // is addressable.
        let (commit, path) = match trimmed.split_once('/') {
            Some((commit, path)) => (commit, path),
            None => (trimmed, ""),
        };
        let sha = commit
            .split_whitespace()
            .next()
            .unwrap_or(commit)
            .to_string();
        Location::In {
            sha,
            path: path.to_string(),
        }
    }

    /// The `#git` segment's tail of a path, or `None` when it is not one.
    fn tail_of(path: &VfsPath) -> Option<PathBuf> {
        let (kind, tail) = path.segments().last()?;
        (*kind == BackendKind::Git).then(|| tail.clone())
    }

    /// The commit list, as directory rows.
    fn list_commits(&self) -> Result<Vec<Entry>> {
        let commits = crate::git::history(&self.dir)?;
        Ok(commits
            .into_iter()
            .map(|commit| {
                // The row name carries the subject after the id, so the
                // listing is readable, and the id stays the addressable head
                // of it: `locate` splits the sha back off the front. A commit
                // subject with a `/` in it cannot mislead the path, because the
                // sha is taken before the first space and a sha has no space.
                let subject = commit.subject.replace('/', " ");
                let name = if subject.is_empty() {
                    commit.short.clone()
                } else {
                    format!("{}  {subject}", commit.short)
                };
                let mut entry = Entry::dir(name);
                entry.kind = EntryKind::Dir;
                entry.mtime = commit.time;
                // The row's *path* is the sha alone - the subject stays in the
                // name for the eye but never reaches the path, which otherwise
                // read `…#git/abc123  Fix the bug/src/lib.rs`. Set here rather
                // than left to `current_path`'s name-join, exactly as a virtual
                // listing sets a row's real home.
                entry.location = Some(
                    VfsPath::local(&self.dir)
                        .with_segment(BackendKind::Git, format!("/{}", commit.short)),
                );
                entry
            })
            .collect())
    }

    /// The files a commit changed, each carrying what it did to them.
    ///
    /// A commit's own listing is its diff, not its tree: standing in a revision
    /// to be told that every file in the repository is there says nothing the
    /// working directory did not already say, and it buries the handful of rows
    /// the commit is actually about. The rows are paths rather than names,
    /// because a diff is flat - two `mod.rs` in one commit are two different
    /// files and only their paths tell them apart - and each row's `location`
    /// points at the blob inside this revision, so `Enter` and `F3` reach the
    /// file as this commit left it. Descending into a directory still lists the
    /// tree, which is what a directory row means.
    fn list_changed(&self, sha: &str) -> Result<Vec<Entry>> {
        let rows = crate::git::changed(&self.dir, sha)?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let mut entry = Entry::file(row.path.clone());
                entry.kind = EntryKind::File;
                entry.size = row.size;
                entry.git_state = Some(row.state);
                entry.location = Some(
                    VfsPath::local(&self.dir)
                        .with_segment(BackendKind::Git, format!("/{sha}/{}", row.path)),
                );
                entry
            })
            .collect())
    }

    /// One level of a commit's tree, directories first.
    ///
    /// The full tree, not the changed files: a commit browses like the working
    /// directory did at that point, so a `mod.rs` sits in its own folder
    /// rather than flattened to the root beside six others of the same name.
    fn list_tree(&self, sha: &str, path: &str) -> Result<Vec<Entry>> {
        let rows = crate::git::tree_at(&self.dir, sha, path)?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let mut entry = if row.is_dir {
                    Entry::dir(row.name)
                } else {
                    Entry::file(row.name)
                };
                entry.kind = if row.is_dir {
                    EntryKind::Dir
                } else {
                    EntryKind::File
                };
                entry.size = row.size;
                entry
            })
            .collect())
    }
}

impl Vfs for GitFs {
    fn kind(&self) -> BackendKind {
        BackendKind::Git
    }

    fn column_plan(&self, path: &VfsPath) -> Option<crate::panel::ColumnPlan> {
        use crate::panel::ColumnId;
        let tail = Self::tail_of(path).unwrap_or_default();
        Some(match Self::locate(&tail) {
            // Commits: the name carries the sha and the subject and wants every
            // cell it can get. There is no extension, no size worth the words
            // `<DIR>`, and no permissions on a commit.
            Location::Commits => vec![ColumnId::Name, ColumnId::Date],
            // A commit's files: the row is a path, so the extension is already
            // in it and a column of its own would repeat it. The size is the
            // blob's and worth having; the state is the point of the listing.
            Location::In { .. } => vec![
                ColumnId::Name,
                ColumnId::Size,
                ColumnId::Date,
                ColumnId::GitState,
            ],
        })
    }

    fn read_dir(&self, path: &VfsPath) -> mpsc::Receiver<Result<Entry>> {
        let (tx, rx) = mpsc::channel(READ_DIR_CHANNEL_DEPTH);
        let this = self.clone();
        let tail = Self::tail_of(path).unwrap_or_default();
        tokio::spawn(async move {
            let rows = match GitFs::locate(&tail) {
                Location::Commits => this.list_commits(),
                // A directory in the tree lists; a file lists empty, the way a
                // file elsewhere does. `tree_at` answers empty for a file.
                // The commit's own row is its diff; a directory inside it is
                // still a directory.
                Location::In { sha, path } if path.is_empty() => this.list_changed(&sha),
                Location::In { sha, path } => this.list_tree(&sha, &path),
            };
            match rows {
                Ok(rows) => {
                    for entry in rows {
                        if tx.send(Ok(entry)).await.is_err() {
                            return;
                        }
                    }
                }
                Err(err) => {
                    let _ = tx.send(Err(err)).await;
                }
            }
        });
        rx
    }

    fn stat(&self, path: &VfsPath) -> Result<Entry> {
        let tail = Self::tail_of(path).unwrap_or_default();
        match Self::locate(&tail) {
            Location::Commits => Ok(Entry::dir("git")),
            Location::In { sha, path } if path.is_empty() => Ok(Entry::dir(sha)),
            Location::In { sha, path } => {
                let name = path.rsplit('/').next().unwrap_or(&path).to_string();
                if crate::git::is_dir_at(&self.dir, &sha, &path) {
                    return Ok(Entry::dir(name));
                }
                let bytes = crate::git::file_at(&self.dir, &sha, &path)?;
                let mut entry = Entry::file(name);
                entry.size = bytes.len() as u64;
                Ok(entry)
            }
        }
    }

    fn open_read(&self, path: &VfsPath) -> Result<Box<dyn std::io::Read + Send>> {
        let tail = Self::tail_of(path).unwrap_or_default();
        match Self::locate(&tail) {
            Location::In { sha, path: inner } if !inner.is_empty() => {
                let bytes = crate::git::file_at(&self.dir, &sha, &inner)?;
                Ok(Box::new(std::io::Cursor::new(bytes)))
            }
            _ => Err(Error::msg(format!("{path}: a commit is not a file"))),
        }
    }

    fn open_seek(&self, path: &VfsPath) -> Result<Box<dyn ReadSeek + Send>> {
        let tail = Self::tail_of(path).unwrap_or_default();
        match Self::locate(&tail) {
            Location::In { sha, path: inner } if !inner.is_empty() => {
                // A git blob is read whole from the object store anyway, so a
                // seekable reader over the bytes is free and lets the viewer
                // use its faster mode.
                let bytes = crate::git::file_at(&self.dir, &sha, &inner)?;
                Ok(Box::new(std::io::Cursor::new(bytes)))
            }
            _ => Err(Error::msg(format!("{path}: a commit is not a file"))),
        }
    }

    fn open_write(&self, path: &VfsPath) -> Result<Box<dyn std::io::Write + Send>> {
        let _ = path;
        Err(Error::msg(
            "git history is read-only; a commit cannot be changed",
        ))
    }

    fn create_dir(&self, _path: &VfsPath) -> Result<()> {
        Err(Error::msg("git history is read-only"))
    }

    fn remove(&self, _path: &VfsPath) -> Result<()> {
        Err(Error::msg("git history is read-only"))
    }

    fn rename(&self, _from: &VfsPath, _to: &VfsPath) -> Result<()> {
        Err(Error::msg("git history is read-only"))
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // Read only, so `F6`/`F8` refuse before the dialog opens and a copy
            // *out* is all that is offered.
            writable: false,
            // The blob is materialised, so a seek is free.
            seekable: true,
            random_access: false,
            has_directories: true,
            atomic_rename: false,
            paged_listing: false,
            can_execute: false,
            links: false,
            settable_mode: false,
            latency: LatencyClass::Local,
        }
    }
}

#[cfg(test)]
#[path = "git_tests.rs"]
mod tests;
