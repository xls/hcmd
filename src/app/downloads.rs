//! The downloads folder and the request that fills it.
//!
//! Files a [`crate::ops::JobKind::Download`] fetches land in a per-process
//! directory under the system temp folder, created on first use and removed
//! when the program ends - a staging area, not a store, cleaned up like the
//! archive session cache. Keeping a download means copying it out of here into
//! a real directory, which is `F5` like anything else.
//!
//! The URL is turned into a job here, in the one place, so every way of asking
//! for a download - the `Ctrl+D` prompt and the viewer's links - reaches the
//! same [`App::request_download`] and produces the same job.

use std::path::{Path, PathBuf};

use crate::app::App;
use crate::ops::{DownloadRequest, JobKind, JobSpec};
use crate::vfs::VfsPath;

/// The prefix a downloads temp directory carries, matching the archive
/// session's convention so the same kind of orphan sweep could find one.
const PREFIX: &str = "hcmd-downloads-";

/// The per-process downloads folder: created on first download, removed on
/// drop.
#[derive(Debug, Default)]
pub struct Downloads {
    /// `None` until the first download creates the directory.
    root: Option<PathBuf>,
}

impl Downloads {
    /// The folder, created if it does not exist yet.
    fn dir(&mut self) -> std::io::Result<&Path> {
        if self.root.is_none() {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let root =
                std::env::temp_dir().join(format!("{PREFIX}{}-{nanos:x}", std::process::id()));
            std::fs::create_dir_all(&root)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let _ = std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700));
            }
            self.root = Some(root);
        }
        self.root
            .as_deref()
            .ok_or_else(|| std::io::Error::other("the downloads folder could not be resolved"))
    }

    /// The folder if it has been created, for a listing to show it.
    #[must_use]
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }
}

impl Drop for Downloads {
    /// Empty the staging area. A best-effort remove: a file held open by an
    /// external program the user launched is the operating system's to release,
    /// and a leftover directory is swept next time rather than fatal now.
    fn drop(&mut self) {
        if let Some(root) = &self.root {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

/// The filename a URL downloads under: its last path segment, without the query
/// or fragment, sanitised so it cannot climb out of the downloads folder.
fn download_name(url: &str) -> String {
    // Drop the query and fragment, then the scheme and authority: the name is
    // in the path, so `https://host` and `https://host/` have none and fall
    // back rather than taking the host for a filename.
    let no_query = url.split(['?', '#']).next().unwrap_or(url);
    let after_scheme = no_query
        .split_once("://")
        .map_or(no_query, |(_scheme, rest)| rest);
    let path = after_scheme
        .split_once('/')
        .map_or("", |(_host, path)| path);
    let raw = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    // What is left came off the network and is about to be joined onto the
    // downloads folder; refuse a shape that could climb out of it rather than
    // the path.
    let clean = raw.trim();
    if clean.is_empty() || clean == "." || clean == ".." || clean.contains('\\') {
        return "download".to_string();
    }
    clean.to_string()
}

/// A name that is not already taken in `dir`: `file`, then `file (2)`, `file
/// (3)`, keeping any extension.
fn unique_name(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), format!(".{ext}")),
        _ => (name.to_string(), String::new()),
    };
    for n in 2..10_000u32 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    // Ten thousand of the same name is not a real case; fall back to the plain
    // one rather than loop forever.
    dir.join(name)
}

impl App {
    /// Fetch `url` into the downloads folder as a background job.
    ///
    /// The one entry point for a download, whatever asked for it. It validates
    /// the scheme, makes the folder, chooses a free name and queues a
    /// [`JobKind::Download`]; the job reports progress and can be sent to the
    /// background exactly as a copy can.
    pub fn request_download(&mut self, url: &str) {
        let url = url.trim();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            self.message = Some("a download needs an http:// or https:// URL".to_string());
            return;
        }
        let dir = match self.downloads.dir() {
            Ok(dir) => dir.to_path_buf(),
            Err(e) => {
                self.message = Some(format!("the downloads folder could not be created: {e}"));
                return;
            }
        };
        let name = download_name(url);
        let target = unique_name(&dir, &name);
        let display = target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&name)
            .to_string();
        let mut spec = JobSpec::new(JobKind::Download, Vec::new(), Some(VfsPath::local(target)));
        spec.options.download = Some(DownloadRequest {
            url: url.to_string(),
            resume: false,
        });
        self.request_job(spec);
        self.message = Some(format!("downloading {display}"));
    }
}

#[cfg(test)]
#[path = "downloads_tests.rs"]
mod tests;
