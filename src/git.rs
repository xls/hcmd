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

#[cfg(test)]
#[path = "git_tests.rs"]
mod tests;
