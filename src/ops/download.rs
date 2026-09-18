//! `JobKind::Download` - fetch a URL into the downloads folder.
//!
//! The one job whose source is a URL rather than a path: it reads
//! [`crate::ops::JobOptions::download`] for the URL and [`JobSpec::dest`] for
//! the local file to write, then streams the transfer through
//! [`crate::net::download`], reporting each chunk to the [`JobContext`]. That
//! is the whole of it - progress bar, background, queue and cancel all come
//! from the job machinery a copy already uses, so a download is a copy with a
//! URL for a source and nothing bespoke around it.
//!
//! # A file already there
//!
//! Asked about exactly as a copy asks, through [`JobContext::ask`] and the
//! same conflict dialog, so the answers mean what they mean for a copy:
//! `Overwrite` fetches afresh, `Rename` fetches beside it under a free name,
//! `Skip` fetches nothing. The one reading of its own is **`Append`, which
//! resumes**: the partial file is continued from where it stopped with a
//! `Range` request rather than appended to blindly, because that is the only
//! sense appending has for a download. A conflict policy already chosen -
//! `ops.confirm_overwrite = false`, or an "apply to all" - is honoured without
//! asking, again as a copy would.
//!
//! A cancel leaves the partial file in place, because the point of streaming
//! to a real file with `Range` is that the next attempt resumes it rather than
//! starting over.

use std::path::{Path, PathBuf};

use super::{ConflictChoice, ConflictRequest, Decision, JobContext, JobKind, JobSpec};
use crate::net::{self, DownloadStatus};
use crate::vfs::{Vfs, VfsPath};

/// What to do about a file already at the destination, once decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Fetch into `path`, resuming it when `resume` is set.
    Fetch {
        /// Where the bytes go.
        path: PathBuf,
        /// Continue the partial file with a `Range` request.
        resume: bool,
    },
    /// Fetch nothing; the job counts one skipped item.
    Skip,
    /// The user cancelled the question, or the dialog went away.
    Cancelled,
}

/// A name not already taken beside `dir/name`: `file`, then `file (2)`,
/// `file (3)`, keeping any extension. The download's own rule for
/// [`ConflictChoice::Rename`] with no name typed.
#[must_use]
pub fn free_name(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), format!(".{ext}")),
        _ => (name.to_string(), String::new()),
    };
    for n in 2..10_000_u32 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    // Ten thousand of the same name is not a real case; fall back to the plain
    // one rather than loop forever.
    dir.join(name)
}

/// Turn a conflict answer into what the fetch does.
///
/// Pure, so the mapping can be tested without a server: `Append` is the
/// resume, the three overwrites all fetch afresh - a download has no source
/// date or size to compare against until it has been fetched, so "if newer"
/// and "if different" can only mean "yes" here - and `Rename` takes the typed
/// name or a free one beside `path`.
#[must_use]
pub fn outcome_for(path: &Path, choice: ConflictChoice, rename_to: Option<&str>) -> Outcome {
    match choice {
        ConflictChoice::Append => Outcome::Fetch {
            path: path.to_path_buf(),
            resume: true,
        },
        ConflictChoice::Overwrite
        | ConflictChoice::OverwriteIfNewer
        | ConflictChoice::OverwriteIfDifferentSize => Outcome::Fetch {
            path: path.to_path_buf(),
            resume: false,
        },
        ConflictChoice::Skip => Outcome::Skip,
        ConflictChoice::Rename => {
            let dir = path.parent().unwrap_or(path);
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("download");
            let path = match rename_to.map(str::trim).filter(|n| !n.is_empty()) {
                Some(typed) => dir.join(typed),
                None => free_name(dir, name),
            };
            Outcome::Fetch {
                path,
                resume: false,
            }
        }
    }
}

/// Decide what to do about `path`, which already exists, asking through the
/// job's conflict channel unless a policy is already in force.
fn settle_existing(
    dest: &VfsPath,
    path: &Path,
    meta: &std::fs::Metadata,
    spec: &JobSpec,
    ctx: &mut JobContext,
) -> Outcome {
    if let Some(choice) = spec.options.conflict {
        return outcome_for(path, choice, None);
    }
    let request = ConflictRequest {
        // A copy names the destination on both sides too: the dialog shows
        // the path once and describes what is there against what is coming.
        source: dest.clone(),
        dest: dest.clone(),
        // Not known until the server answers; the dialog reads a zero as
        // "no size to compare", which is the truth of it.
        source_size: 0,
        dest_size: meta.len(),
        source_mtime: None,
        dest_mtime: meta.modified().ok(),
        both_dirs: false,
        dest_is_dir: meta.is_dir(),
        resumable: true,
    };
    match ctx.ask(request) {
        Some(Decision::Conflict {
            choice, rename_to, ..
        }) => outcome_for(path, choice, rename_to.as_deref()),
        Some(Decision::Cancel) | None => Outcome::Cancelled,
    }
}

/// Fetch the URL in `spec.options.download` into `spec.dest`.
pub fn run(_vfs: &dyn Vfs, spec: &JobSpec, ctx: &mut JobContext) {
    debug_assert_eq!(spec.kind, JobKind::Download);
    // One file, size unknown until the server answers; `set_file` fills the
    // total in once `net::download` has it.
    ctx.start(1, 0);

    let Some(request) = spec.options.download.as_ref() else {
        ctx.fail(&VfsPath::local_root(), "no download was described");
        return;
    };
    let Some(dest) = spec.dest.as_ref() else {
        ctx.fail(&VfsPath::local_root(), "the download has no destination");
        return;
    };
    let Some(path) = dest.local_path() else {
        ctx.fail(dest, "a download can only be written to a local folder");
        return;
    };

    // Something already there is a question, answered the way a copy answers
    // it; the request's own `resume` flag stands when there is nothing to ask.
    let (path, resume) = match std::fs::metadata(path).ok() {
        None => (path.to_path_buf(), request.resume),
        Some(meta) => match settle_existing(dest, path, &meta, spec, ctx) {
            Outcome::Fetch { path, resume } => (path, resume),
            Outcome::Skip => {
                ctx.add_skipped();
                return;
            }
            Outcome::Cancelled => return,
        },
    };

    let display = path.display().to_string();
    let mut announced = false;
    let mut prev = 0_u64;
    // `net::download` reports cumulative bytes (offset included on a resume) and
    // the total once known; the job wants a total up front and deltas after, so
    // set the file on the first report and add the difference each time.
    let result = net::download(&request.url, &path, resume, &mut |done, total| {
        if !announced {
            ctx.set_file(&display, total.unwrap_or(0));
            announced = true;
        }
        let delta = done.saturating_sub(prev);
        prev = done;
        ctx.add_bytes(delta)
    });

    match result {
        Ok(DownloadStatus::Completed) => {
            if !announced {
                ctx.set_file(&display, 0);
            }
            ctx.add_file();
        }
        // A cancel is not a failure: the partial file stays for a resume.
        Ok(DownloadStatus::Cancelled) => {}
        Err(err) => ctx.fail(&VfsPath::local(path), err),
    }
}

#[cfg(test)]
#[path = "download_tests.rs"]
mod tests;
