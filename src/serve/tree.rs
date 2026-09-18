//! What the share holds, and where a URL path lands in it.
//!
//! The share is the selection: a handful of files and folders that need not
//! share a parent. Its root is a made-up directory listing them by name, and
//! below a folder the real tree is walked. Every request path is resolved
//! here, once, into either a place in that tree or a refusal - and a refusal
//! is decided on the path's *shape* before anything on disk is looked at, so
//! `..` and its friends never reach the filesystem.

use std::path::{Path, PathBuf};

/// One thing that was selected: what it is called on the share, and where it
/// really is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    /// The name it is served under - the entry's own file name, made unique
    /// when two selected entries share one.
    pub name: String,
    /// Where it is on disk.
    pub path: PathBuf,
}

/// Where a request path lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// The share's own root: the list of everything selected.
    Index,
    /// A path under one of the roots. `is_root` says it is the root itself.
    Path {
        /// The share name it is under.
        root: String,
        /// The filesystem path.
        path: PathBuf,
        /// The URL path that names it, `/`-separated and ending in `/` for
        /// a directory, for building links and hrefs.
        url: String,
    },
    /// A shape that could leave the share: `..`, an empty segment, a NUL.
    Forbidden,
    /// No root of that name.
    NotFound,
}

/// Name the selected paths for the share: by file name, numbered when two
/// collide, and skipping anything with no name at all (a filesystem root).
#[must_use]
pub fn roots(selected: &[PathBuf]) -> Vec<Root> {
    let mut out: Vec<Root> = Vec::new();
    for path in selected {
        let Some(base) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let mut name = base.to_string();
        let mut n = 2_u32;
        while out.iter().any(|r| r.name == name) {
            name = format!("{base} ({n})");
            n = n.saturating_add(1);
        }
        out.push(Root {
            name,
            path: path.clone(),
        });
    }
    out
}

/// Is this a segment a URL path may carry into the tree?
fn segment_ok(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && !segment.contains('\0')
        && !segment.contains('\\')
}

/// Resolve a decoded request path against the roots.
#[must_use]
pub fn resolve(roots: &[Root], path: &str) -> Resolved {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        return Resolved::Index;
    }
    let wants_dir = trimmed.ends_with('/');
    let segments: Vec<&str> = trimmed.trim_end_matches('/').split('/').collect();
    if !segments.iter().all(|s| segment_ok(s)) {
        return Resolved::Forbidden;
    }
    let Some((first, rest)) = segments.split_first() else {
        return Resolved::Index;
    };
    let Some(root) = roots.iter().find(|r| r.name == *first) else {
        return Resolved::NotFound;
    };
    let mut fs_path = root.path.clone();
    for segment in rest {
        fs_path.push(segment);
    }
    let mut url = String::from("/");
    url.push_str(&super::http::percent_encode(first));
    for segment in rest {
        url.push('/');
        url.push_str(&super::http::percent_encode(segment));
    }
    if wants_dir {
        url.push('/');
    }
    Resolved::Path {
        root: root.name.clone(),
        path: fs_path,
        url,
    }
}

/// Does `path`, once symlinks are followed, still sit under `root`? A symlink
/// inside the share pointing out of it is served as nothing rather than as
/// the thing it points to.
#[must_use]
pub fn stays_inside(root: &Path, path: &Path) -> bool {
    match (root.canonicalize(), path.canonicalize()) {
        (Ok(root), Ok(real)) => real.starts_with(&root),
        _ => false,
    }
}

#[cfg(test)]
#[path = "tree_tests.rs"]
mod tests;
