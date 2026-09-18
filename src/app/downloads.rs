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

/// Does a URL look like it names a file to fetch rather than a page to read?
///
/// Judged by the path's extension, which is all a link tells you before it is
/// fetched: `report.pdf` and `tool.tar.gz` are files; `index.html`, `/docs/`
/// and `?q=x` are pages. A file link is offered as a download and a page link
/// goes to the browser, so a wrong guess costs one extra keystroke, not data.
#[must_use]
pub fn url_is_file(url: &str) -> bool {
    let name = download_name(url);
    if name == "download" {
        return false;
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => !matches!(
            ext.to_ascii_lowercase().as_str(),
            "html" | "htm" | "xhtml" | "shtml" | "php" | "asp" | "aspx" | "jsp" | "cgi"
        ),
        _ => false,
    }
}

/// The free-name rule, kept reachable from this module's tests; the runner
/// owns it now, since `Rename` is answered there.
#[cfg(test)]
fn unique_name(dir: &Path, name: &str) -> PathBuf {
    crate::ops::download::free_name(dir, name)
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
        // The plain name, on purpose: a second fetch of the same URL lands on
        // the same file, which is what lets the job ask "resume or overwrite?"
        // instead of quietly making a `file (2)`.
        let target = dir.join(&name);
        let dest = VfsPath::local(target);
        // One download per destination at a time. A job already fetching into
        // this file is the one to wait for (or cancel), not to race.
        let busy = self.jobs.rows().iter().any(|row| {
            row.kind == JobKind::Download
                && !row.is_finished()
                && self.jobs.spec(row.id).and_then(|spec| spec.dest.as_ref()) == Some(&dest)
        });
        if busy {
            self.message = Some(format!("already downloading {name} - see the job queue"));
            return;
        }
        let display = name.clone();
        let mut spec = JobSpec::new(JobKind::Download, Vec::new(), Some(dest));
        spec.options.download = Some(DownloadRequest {
            url: url.to_string(),
            resume: false,
        });
        self.request_job(spec);
        self.message = Some(format!("downloading {display}"));
    }

    /// What a link in the viewer asks for: a file link is offered as a
    /// download, anything else opens in the browser.
    ///
    /// The viewer's whole part is to hand the target here - it does not know
    /// about jobs, folders or browsers - so a link and the `Ctrl+D` prompt end
    /// in the same [`App::request_download`]. Named for the web, because
    /// `request_link` is already the symlink job.
    pub fn follow_web_link(&mut self, target: &str) {
        let target = target.trim();
        if !(target.starts_with("http://") || target.starts_with("https://")) {
            // A relative path, an anchor, a `mailto:`: nothing to fetch or
            // browse to from here.
            self.message = Some(format!("not a web link: {target}"));
            return;
        }
        if !url_is_file(target) {
            self.open_link_in_browser(target);
            return;
        }
        self.pending_link_download = Some(target.to_string());
        let name = download_name(target);
        self.push_dialog(Box::new(
            crate::dialog::ConfirmDialog::new(
                crate::input::DialogId::DownloadLink,
                "Download",
                vec![format!("Download {name}?"), target.to_string()],
            )
            .with_buttons("Download", "Cancel"),
        ));
    }

    /// Hand a link to the desktop browser, and say so on the status line.
    pub fn open_link_in_browser(&mut self, target: &str) {
        self.message = Some(match crate::ops::open::desktop_open_url(target) {
            Ok(()) => format!("opened in the browser: {target}"),
            Err(e) => format!("could not open {target}: {e}"),
        });
    }

    /// The answer to the download prompt a link raised.
    pub fn answer_link_download(&mut self, yes: bool) {
        let Some(url) = self.pending_link_download.take() else {
            return;
        };
        if yes {
            self.request_download(&url);
        }
    }

    /// Show the downloads folder in the active panel.
    ///
    /// The folder is a real local directory, so this is an ordinary navigate;
    /// it is created first so the panel has something to list even before the
    /// first download has landed.
    pub fn show_downloads(&mut self) {
        match self.downloads.dir() {
            Ok(dir) => {
                let path = VfsPath::local(dir.to_path_buf());
                self.navigate(self.active_side, path);
            }
            Err(e) => {
                self.message = Some(format!("the downloads folder could not be opened: {e}"));
            }
        }
    }
}

#[cfg(test)]
#[path = "downloads_tests.rs"]
mod tests;
