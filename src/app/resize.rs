//! What `Shift+R` asks for, and the header read behind it.
//!
//! # Why it is not done where the key is pressed
//!
//! The resize dialog says what it is about to work on: the file name, its
//! pixel size, its format and its size on disk. "50%" means nothing without
//! the first of those, and the pixel size is only in the file's own header -
//! which means a read, and inside an archive or on a remote panel a
//! decompression or a network round trip. `dispatch` performs no I/O, so the
//! keystroke queues a request and the event loop performs it, the same shape
//! [`crate::app::fileinfo`] uses for `Shift+F9`.
//!
//! # Why `image` reads the header rather than the templates
//!
//! [`crate::viewer::fileinfo::describe`] is the program's one answer to "what
//! is this file", and it was the obvious candidate. It cannot answer this
//! question: a JPEG's dimensions live in its `SOF0` marker, an unbounded
//! distance into the file behind however much EXIF the camera wrote, and the
//! flat field templates cannot reach it - `templates/image/jpeg.toml` has no
//! `Dimensions` line at all, and JPEG is the format a resizer is pointed at
//! most. So the numbers come from `image`'s own reader, which is also the
//! decoder that will do the work: the size on the dialog is by construction
//! the size being resized, rather than a second opinion about it.
//!
//! It reads from the head this module already pulled through the [`Vfs`], not
//! from a path, so an image inside a zip or on an SFTP host is described the
//! same way as one on this machine.
//!
//! A header that cannot be read is not an error anybody needs to see. The
//! dialog still opens and says so on that line, because a file this program
//! cannot measure is still a file the user may be asking it to convert.

use std::io::{Cursor, Read as _};
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::app::App;
use crate::vfs::{Vfs, VfsPath};
use crate::viewer::fileinfo::HEAD_BYTES;

/// How deep the answer's channel is.
///
/// One request is outstanding at a time - a second `Shift+R` replaces the
/// first rather than queueing behind it - so this is never full. It is bounded
/// because every channel in the event loop is.
pub const RESIZE_CHANNEL_DEPTH: usize = 2;

/// What the keystroke knew, on its way to the event loop.
#[derive(Debug, Clone)]
pub struct ResizeRequest {
    /// The first of the operands, which is the one the header describes and
    /// the one the name preview is about.
    pub path: VfsPath,
    /// Its name, as the listing shows it.
    pub name: String,
    /// Its size in bytes, from the listing rather than from a second `stat`.
    pub size: u64,
    /// How many operands there are in total.
    pub count: usize,
    /// Where the output goes, for display: the other panel's directory, which
    /// is the convention `F5` and `F6` already follow.
    pub destination: String,
}

/// What the dialog is opened with: the selection, described.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResizeSubject {
    /// The first operand's name.
    pub name: String,
    /// Its size in bytes.
    pub size: u64,
    /// How many images the job will act on.
    pub count: usize,
    /// What `image` recognises the bytes as, `None` when nothing did.
    pub format: Option<image::ImageFormat>,
    /// Its pixel size, `None` when the header could not be read.
    pub dimensions: Option<(u32, u32)>,
    /// Where the output goes, for display. Empty where it is not known, in
    /// which case the dialog says the name alone rather than an empty path.
    pub destination: String,
}

impl ResizeSubject {
    /// A selection nothing could be read about, which still opens the dialog.
    pub fn unknown(name: impl Into<String>, size: u64, count: usize) -> Self {
        Self {
            name: name.into(),
            size,
            count,
            format: None,
            dimensions: None,
            destination: String::new(),
        }
    }

    /// The same subject, with the directory its output will be written to.
    #[must_use]
    pub fn into_directory(mut self, destination: impl Into<String>) -> Self {
        self.destination = destination.into();
        self
    }
}

impl App {
    /// Queue the resize dialog for the operands the keystroke was about.
    ///
    /// A second press replaces the first: the answer to the older one would
    /// open a dialog about a selection that has already changed.
    pub fn request_resize(&mut self, request: ResizeRequest) {
        self.pending_resize = Some(request);
    }

    /// The queued request, for the event loop and for tests.
    pub fn take_pending_resize(&mut self) -> Option<ResizeRequest> {
        self.pending_resize.take()
    }

    /// Perform the queued read, off the event loop.
    pub fn service_resize(&mut self, tx: &mpsc::Sender<ResizeSubject>) {
        let Some(request) = self.take_pending_resize() else {
            return;
        };
        let vfs = Arc::clone(&self.vfs);
        let tx = tx.clone();
        tokio::task::spawn_blocking(move || {
            let head = read_head(vfs.as_ref(), &request.path);
            let _ = tx.blocking_send(describe(&request, &head));
        });
    }
}

/// The front of the file, or nothing at all.
///
/// Bounded by [`HEAD_BYTES`] whatever the listing's size claims, so a backend
/// that lies about a length cannot turn this into an unbounded read. It is the
/// same bound `Shift+F9` reads under, and it is generous for this purpose:
/// every image header this program can decode is inside it.
fn read_head(vfs: &dyn Vfs, path: &VfsPath) -> Vec<u8> {
    let Ok(reader) = vfs.open_read(path) else {
        return Vec::new();
    };
    let mut head = Vec::new();
    // A short read is the answer for a short file and a failed one is the
    // answer for a file that stopped being readable: both leave whatever
    // arrived, and an image header that is not in it is one this cannot
    // measure, which the dialog says.
    let _ = reader.take(HEAD_BYTES as u64).read_to_end(&mut head);
    head
}

/// What the head turned out to be.
///
/// Pure, so a test can put bytes in front of it without a filesystem.
pub fn describe(request: &ResizeRequest, head: &[u8]) -> ResizeSubject {
    let mut subject = ResizeSubject::unknown(&request.name, request.size, request.count)
        .into_directory(&request.destination);
    let Ok(reader) = image::ImageReader::new(Cursor::new(head)).with_guessed_format() else {
        return subject;
    };
    subject.format = reader.format();
    // `into_dimensions` consumes the reader and reads no pixels: for every
    // format here it is the header and, for JPEG, the markers up to `SOF`.
    subject.dimensions = reader.into_dimensions().ok();
    subject
}

#[cfg(test)]
#[path = "resize_tests.rs"]
mod tests;
