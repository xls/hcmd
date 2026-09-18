//! `JobKind::Download` - fetch a URL into the downloads folder.
//!
//! The one job whose source is a URL rather than a path: it reads
//! [`crate::ops::JobOptions::download`] for the URL and the resume flag and
//! [`JobSpec::dest`] for the local file to write, then streams the transfer
//! through [`crate::net::download`], reporting each chunk to the
//! [`JobContext`]. That is the whole of it - progress bar, background, queue
//! and cancel all come from the job machinery a copy already uses, so a
//! download is a copy with a URL for a source and nothing bespoke around it.
//!
//! A cancel leaves the partial file in place, because the point of streaming to
//! a real file with `Range` is that the next attempt resumes it rather than
//! starting over.

use super::{JobContext, JobKind, JobSpec};
use crate::net::{self, DownloadStatus};
use crate::vfs::{Vfs, VfsPath};

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

    let display = dest.to_string();
    let mut announced = false;
    let mut prev = 0u64;
    // `net::download` reports cumulative bytes (offset included on a resume) and
    // the total once known; the job wants a total up front and deltas after, so
    // set the file on the first report and add the difference each time.
    let result = net::download(&request.url, path, request.resume, &mut |done, total| {
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
        Err(err) => ctx.fail(dest, err),
    }
}
