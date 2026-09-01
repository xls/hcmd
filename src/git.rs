//! What git has for a file, so `F3` can show what changed.
//!
//! Only ever **reads**, and only ever the object store. Nothing here writes to
//! a repository, stages anything, or runs `git`: the rule that this program
//! starts no subprocess for its own work applies to reading a blob as much as
//! to listing a directory, so this reads the object database itself.
//!
//! # Why a library rather than the two easy cases
//!
//! A loose object is a zlib stream behind a two-line header, and `flate2` is
//! already linked, so "read HEAD's copy of this file" looks like an afternoon.
//! It is not. The blob of a file that was committed at any point in the past
//! is almost always in a packfile, behind an index, a fan-out table and a
//! chain of deltas that have to be applied in order. Getting that subtly wrong
//! produces a diff against the wrong bytes, which is worse than no diff.

use std::path::{Path, PathBuf};

/// What git says about a file, for the status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Tracked, and the working copy differs from `HEAD`.
    Modified,
    /// Tracked, and the same as `HEAD`.
    Unmodified,
}

impl State {
    /// How the status line spells it.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Modified => "git modified",
            Self::Unmodified => "git unmodified",
        }
    }
}

/// What git knows about one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadBlob {
    /// The file's contents as of `HEAD`.
    pub bytes: Vec<u8>,
    /// How to name the left-hand side of the diff: `a.txt (HEAD)`.
    pub label: String,
}

/// The file's contents as of `HEAD`, or `None` when git has nothing to say.
///
/// `None` rather than an error for every ordinary "there is nothing to show"
/// case - the file is not in a repository, the repository has no commits yet,
/// or the file is untracked. None of those is a failure and none of them
/// should reach the user as one; the caller says "not tracked" and moves on.
///
/// An error is reserved for a repository that exists and could not be read,
/// which is a real problem worth a sentence.
pub fn head_blob(path: &Path) -> crate::Result<Option<HeadBlob>> {
    let Some(parent) = path.parent() else {
        return Ok(None);
    };
    let Ok(repo) = gix::discover(parent) else {
        return Ok(None);
    };
    let Some(workdir) = repo.workdir() else {
        return Ok(None);
    };
    // The path git knows this file by, which is relative to the work tree and
    // not to wherever the panel happens to be standing.
    let absolute = absolute_of(path);
    let Ok(relative) = absolute.strip_prefix(workdir) else {
        return Ok(None);
    };

    // A repository with no commits has a `HEAD` that resolves to nothing. That
    // is a new repository rather than a broken one.
    let Ok(head) = repo.head_commit() else {
        return Ok(None);
    };
    let Ok(mut tree) = head.tree() else {
        return Ok(None);
    };
    let Ok(Some(entry)) = tree.peel_to_entry_by_path(relative) else {
        // In the tree's absence the file is untracked, which is the common
        // case for a file somebody is looking at in a working copy.
        return Ok(None);
    };
    let object = entry
        .object()
        .map_err(|err| crate::error::Error::msg(format!("git object: {err}")))?;

    Ok(Some(HeadBlob {
        bytes: object.data.clone(),
        label: format!(
            "{} (HEAD)",
            relative.file_name().map_or_else(
                || relative.display().to_string(),
                |n| n.to_string_lossy().into_owned()
            )
        ),
    }))
}

/// `path` made absolute without touching the filesystem for a file that is
/// already absolute.
fn absolute_of(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

/// One commit, reduced to what a panel row shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// The full hex id.
    pub id: String,
    /// The abbreviated id, for the row name.
    pub short: String,
    /// The first line of the message.
    pub subject: String,
    /// The author's name.
    pub author: String,
    /// When it was committed.
    pub time: Option<std::time::SystemTime>,
}

/// One file a commit touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed {
    /// The path, relative to the repository root.
    pub path: String,
    /// Size of the file's content at this commit, as the tree records it.
    pub size: u64,
    /// What this commit did to it. The diff already answers this; keeping it
    /// is what lets the listing say more than "this file was touched".
    pub state: FileState,
}

/// The most commits a history listing holds.
///
/// A repository is allowed to have more commits than anybody wants to hold in
/// a panel at once, so the walk stops here, the way every other listing in
/// this program is bounded.
pub const MAX_COMMITS: usize = 50_000;

/// The repository that contains `path`, if any.
///
/// The directory a panel is standing in, walked up to its `.git`. `None` for a
/// path that is not in a repository, which is not an error: it is the common
/// case and the caller says so rather than reporting a failure.
fn repo_at(path: &Path) -> Option<gix::Repository> {
    gix::discover(path).ok()
}

/// Every commit reachable from `HEAD`, newest first.
pub fn history(dir: &Path) -> crate::Result<Vec<Commit>> {
    let Some(repo) = repo_at(dir) else {
        return Ok(Vec::new());
    };
    let Ok(head) = repo.head_commit() else {
        // A repository with no commits yet.
        return Ok(Vec::new());
    };
    let walk = repo
        .rev_walk([head.id])
        .all()
        .map_err(|err| crate::error::Error::msg(format!("git history: {err}")))?;
    let mut out = Vec::new();
    for info in walk {
        if out.len() >= MAX_COMMITS {
            break;
        }
        let Ok(info) = info else { continue };
        let Ok(commit) = repo.find_commit(info.id) else {
            continue;
        };
        let id = commit.id().to_string();
        let message = commit.message_raw_sloppy();
        let subject = String::from_utf8_lossy(message)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        let (author, time) = commit
            .author()
            .map(|a| {
                let secs = a.time().map(|t| t.seconds).unwrap_or(0);
                let time = u64::try_from(secs)
                    .ok()
                    .map(|s| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s));
                (a.name.to_string(), time)
            })
            .unwrap_or_default();
        out.push(Commit {
            short: id.get(..8).unwrap_or(&id).to_string(),
            id,
            subject,
            author,
            time,
        });
    }
    Ok(out)
}

/// The files one commit changed, against its first parent.
///
/// A root commit has no parent, so everything in it is "changed", which is the
/// honest answer: the first commit created every file it holds.
pub fn changed(dir: &Path, sha: &str) -> crate::Result<Vec<Changed>> {
    let Some(repo) = repo_at(dir) else {
        return Ok(Vec::new());
    };
    let Ok(id) = repo.rev_parse_single(sha) else {
        return Err(crate::error::Error::msg(format!("no such commit: {sha}")));
    };
    let Ok(commit) = repo.find_commit(id) else {
        return Err(crate::error::Error::msg(format!("no such commit: {sha}")));
    };
    let Ok(new_tree) = commit.tree() else {
        return Ok(Vec::new());
    };
    let parent = commit.parent_ids().next();
    let old_tree = parent
        .and_then(|p| repo.find_commit(p).ok())
        .and_then(|c| c.tree().ok());

    let changes = repo
        .diff_tree_to_tree(old_tree.as_ref(), Some(&new_tree), None)
        .map_err(|err| crate::error::Error::msg(format!("git diff: {err}")))?;

    let mut out = Vec::new();
    for change in changes {
        let (path, blob, state, mode) = match &change {
            gix::object::tree::diff::ChangeDetached::Addition {
                location,
                id,
                entry_mode,
                ..
            } => (
                location.to_string(),
                Some(*id),
                FileState::Added,
                *entry_mode,
            ),
            gix::object::tree::diff::ChangeDetached::Modification {
                location,
                id,
                entry_mode,
                ..
            } => (
                location.to_string(),
                Some(*id),
                FileState::Modified,
                *entry_mode,
            ),
            gix::object::tree::diff::ChangeDetached::Deletion {
                location,
                entry_mode,
                ..
            } => (location.to_string(), None, FileState::Removed, *entry_mode),
            gix::object::tree::diff::ChangeDetached::Rewrite {
                location,
                id,
                entry_mode,
                ..
            } => (
                location.to_string(),
                Some(*id),
                FileState::Renamed,
                *entry_mode,
            ),
        };
        // A directory that gained or lost its last file shows up here as a
        // change of its own. It is not a file the commit edited, and a listing
        // that offered it would offer a row with nothing to open.
        if mode.is_tree() {
            continue;
        }
        let size = blob
            .and_then(|id| repo.find_object(id).ok())
            .map_or(0, |obj| obj.data.len() as u64);
        out.push(Changed { path, size, state });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// One entry in a commit's tree at some level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// The entry's own name, not its path.
    pub name: String,
    /// A directory, or a file.
    pub is_dir: bool,
    /// The file's size, or zero for a directory.
    pub size: u64,
}

/// List one level of a commit's tree.
///
/// `subpath` is empty for the commit's root and `src/ui` for a directory
/// within it. Directories come first, then files, each sorted, so a commit's
/// tree browses exactly like a directory on disk - which is the whole point of
/// this being a `Vfs`. A file's bytes are read with [`file_at`]; this is only
/// the listing.
pub fn tree_at(dir: &Path, sha: &str, subpath: &str) -> crate::Result<Vec<TreeEntry>> {
    let Some(repo) = repo_at(dir) else {
        return Ok(Vec::new());
    };
    let id = repo
        .rev_parse_single(sha)
        .map_err(|_| crate::error::Error::msg(format!("no such commit: {sha}")))?;
    let commit = repo
        .find_commit(id)
        .map_err(|_| crate::error::Error::msg(format!("no such commit: {sha}")))?;
    let mut tree = commit
        .tree()
        .map_err(|err| crate::error::Error::msg(format!("git tree: {err}")))?;

    // Descend to the sub-tree, when one was asked for.
    let subpath = subpath.trim_matches('/');
    if !subpath.is_empty() {
        let entry = tree
            .peel_to_entry_by_path(std::path::Path::new(subpath))
            .map_err(|err| crate::error::Error::msg(format!("git path: {err}")))?
            .ok_or_else(|| crate::error::Error::msg(format!("{subpath}: not in commit {sha}")))?;
        let object = entry
            .object()
            .map_err(|err| crate::error::Error::msg(format!("git object: {err}")))?;
        // A path that resolves to a file is not a directory to list; the caller
        // reads it instead, so an empty listing is the honest answer.
        let Ok(sub) = object.try_into_tree() else {
            return Ok(Vec::new());
        };
        tree = sub;
    }

    let mut dirs: Vec<TreeEntry> = Vec::new();
    let mut files: Vec<TreeEntry> = Vec::new();
    for entry in tree.iter() {
        let Ok(entry) = entry else { continue };
        let name = entry.inner.filename.to_string();
        let is_dir = entry.inner.mode.is_tree();
        let size = if is_dir {
            0
        } else {
            repo.find_object(entry.inner.oid.to_owned())
                .map_or(0, |obj| obj.data.len() as u64)
        };
        let row = TreeEntry { name, is_dir, size };
        if is_dir {
            dirs.push(row);
        } else {
            files.push(row);
        }
    }
    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    files.sort_by(|a, b| a.name.cmp(&b.name));
    dirs.extend(files);
    Ok(dirs)
}

/// Whether a path inside a commit is a directory.
///
/// Answered by resolving it in the tree: a caller that has to decide "list or
/// read" asks this rather than guessing from the name, because a file with no
/// extension and a directory look alike.
pub fn is_dir_at(dir: &Path, sha: &str, subpath: &str) -> bool {
    let subpath = subpath.trim_matches('/');
    if subpath.is_empty() {
        return true;
    }
    let Some(repo) = repo_at(dir) else {
        return false;
    };
    let Ok(id) = repo.rev_parse_single(sha) else {
        return false;
    };
    let Ok(commit) = repo.find_commit(id) else {
        return false;
    };
    let Ok(mut tree) = commit.tree() else {
        return false;
    };
    tree.peel_to_entry_by_path(std::path::Path::new(subpath))
        .ok()
        .flatten()
        .is_some_and(|e| e.mode().is_tree())
}

/// The working-tree state of a file, relative to the index and HEAD.
///
/// The states omarchy and every other git-aware listing show, in the order a
/// single flag has to pick between them: a file staged **and** then edited is
/// shown as modified, because the edit is the newer fact and the one a reader
/// is about to lose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    /// Tracked, and the working copy differs from what is staged.
    Modified,
    /// A change is staged and the working copy matches it.
    Staged,
    /// In the index but not in `HEAD`: a newly added, staged file.
    Added,
    /// Not tracked at all.
    Untracked,
    /// Gone at this commit: the change deleted it. A working directory never
    /// shows this - a file that is not there has no row - but a commit does,
    /// because "what this commit did" includes what it removed.
    Removed,
    /// Moved or copied from somewhere else at this commit.
    Renamed,
}

impl FileState {
    /// The one-character flag. Chosen to read at a glance and to line up in a
    /// column: no two share a glyph.
    #[must_use]
    pub const fn flag(self) -> char {
        match self {
            Self::Modified => '~',
            Self::Staged => '+',
            Self::Added => 'A',
            Self::Untracked => '?',
            Self::Removed => '-',
            Self::Renamed => 'R',
        }
    }
}

/// The git state of the files **directly in** `dir`, keyed by file name.
///
/// One index read and one HEAD tree, then a stat-and-maybe-hash per file - the
/// same fast path `git status` uses, so an unmodified file costs a `stat` and
/// not a hash. Scoped to the one directory the panel is showing rather than
/// the whole repository, because that is all a listing needs and a monorepo's
/// full status is not free.
///
/// `None` when `dir` is not in a repository, which is not an error: it is most
/// directories, and the caller simply shows no flags.
pub fn dir_status(dir: &Path) -> Option<std::collections::HashMap<String, FileState>> {
    let repo = repo_at(dir)?;
    let workdir = repo.workdir()?.to_path_buf();
    let rel_dir = dir.strip_prefix(&workdir).ok()?;
    let index = repo.index_or_empty().ok()?;
    // HEAD's tree, for telling a staged change from an unstaged one. A new
    // repository has none, and then everything staged is `Added`.
    let head_tree = repo.head_commit().ok().and_then(|c| c.tree().ok());

    let mut out = std::collections::HashMap::new();
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) if m.is_file() => m,
            // Directories and symlinks are not flagged: a directory's state is
            // the sum of what is under it, which one glyph cannot honestly say.
            _ => continue,
        };
        let rel = rel_dir.join(&name);
        let rel_bytes = rel.to_string_lossy().replace('\\', "/");
        let indexed = index.entry_by_path(rel_bytes.as_str().into());
        let Some(indexed) = indexed else {
            out.insert(name, FileState::Untracked);
            continue;
        };
        // Staged: the index differs from HEAD.
        let head_id = head_tree.as_ref().and_then(|tree| {
            tree.clone()
                .peel_to_entry_by_path(&rel)
                .ok()
                .flatten()
                .map(|e| e.oid().to_owned())
        });
        let staged = match head_id {
            Some(id) => id != indexed.id,
            None => true, // no HEAD entry: newly added
        };
        // Modified in the worktree: cheap stat check first, hash only if it
        // could have changed.
        let modified = worktree_differs(&entry.path(), &meta, indexed);
        let state = if modified {
            FileState::Modified
        } else if staged && head_id.is_none() {
            FileState::Added
        } else if staged {
            FileState::Staged
        } else {
            continue; // clean and unstaged: no flag
        };
        out.insert(name, state);
    }
    Some(out)
}

/// Whether a working file differs from its staged version.
///
/// The `git status` fast path: if the file's size and mtime match what the
/// index recorded, it is unmodified and no hash is computed. Only when they
/// differ is the blob hashed and compared, which is the case a listing pays
/// for rarely.
fn worktree_differs(path: &Path, meta: &std::fs::Metadata, indexed: &gix::index::Entry) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let size_matches = u64::from(indexed.stat.size) == meta.size();
    let mtime_matches = i64::from(indexed.stat.mtime.secs) == meta.mtime();
    if size_matches && mtime_matches {
        return false;
    }
    // The stat changed, which is a hint, not proof - `touch` changes mtime
    // without changing content. Hash the blob the way git stores it and
    // compare to the recorded id.
    let Ok(bytes) = std::fs::read(path) else {
        return true;
    };
    let computed = git_blob_id(&bytes);
    computed.as_deref() != Some(indexed.id.as_slice())
}

/// The git object id of a blob, computed by gix's own hasher so there is no
/// second SHA-1 in the tree and no chance of a spelling that disagrees with
/// what git wrote.
fn git_blob_id(bytes: &[u8]) -> Option<Vec<u8>> {
    gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, bytes)
        .ok()
        .map(|id| id.as_slice().to_vec())
}

/// The same file at a commit's first parent, for a diff.
///
/// `None` when the commit is a root (nothing came before) or the file did not
/// exist in the parent (it was added here) - both of which are the honest
/// "there is nothing on the left" answer, which the diff renders as an
/// all-added file.
pub fn file_at_parent(dir: &Path, sha: &str, path: &str) -> Option<Vec<u8>> {
    let repo = repo_at(dir)?;
    let id = repo.rev_parse_single(sha).ok()?;
    let commit = repo.find_commit(id).ok()?;
    let parent = commit.parent_ids().next()?;
    let parent_sha = parent.to_string();
    file_at(dir, &parent_sha, path).ok()
}

/// A file's contents at a particular commit.
pub fn file_at(dir: &Path, sha: &str, path: &str) -> crate::Result<Vec<u8>> {
    let Some(repo) = repo_at(dir) else {
        return Err(crate::error::Error::msg("not in a git repository"));
    };
    let id = repo
        .rev_parse_single(sha)
        .map_err(|_| crate::error::Error::msg(format!("no such commit: {sha}")))?;
    let commit = repo
        .find_commit(id)
        .map_err(|_| crate::error::Error::msg(format!("no such commit: {sha}")))?;
    let mut tree = commit
        .tree()
        .map_err(|err| crate::error::Error::msg(format!("git tree: {err}")))?;
    let entry = tree
        .peel_to_entry_by_path(std::path::Path::new(path))
        .map_err(|err| crate::error::Error::msg(format!("git path: {err}")))?
        .ok_or_else(|| crate::error::Error::msg(format!("{path}: not in commit {sha}")))?;
    let object = entry
        .object()
        .map_err(|err| crate::error::Error::msg(format!("git object: {err}")))?;
    Ok(object.data.clone())
}

#[cfg(test)]
#[path = "git_tests.rs"]
mod tests;
