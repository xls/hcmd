//! The file information dialog: what dispatch asks for, and the read behind it.
//!
//! `Shift+F9` and `Shift+Space` on a panel, `F9` in the viewer. All three want
//! the same thing - the file's own facts, plus what its contents turn out to
//! be - and all three go through here.
//!
//! # Why it is not done where the key is pressed
//!
//! Recognising the contents means reading the front of the file, and
//! [`crate::viewer::fileinfo::HEAD_BYTES`] is 640 KiB of it, because the
//! furthest thing any template looks at is a UDF anchor at byte 524288.
//! Reading that in `dispatch` would put a disk read - or, inside an archive or
//! on a remote panel, a decompression or a network round trip - on the
//! keystroke's own path, which is the one place this program will not put one.
//! So the keystroke queues a request and the event loop performs it, the same
//! shape [`crate::runtime::open_pending_viewer`] and
//! [`crate::app::App::service_update_check`] already use.
//!
//! A read that fails is not an error anybody needs to see. The dialog still
//! opens, with the name, the size and the attributes the panel already knew,
//! and simply says nothing about the contents: that is the honest answer for
//! an unreadable file, and it is the same answer as for a file no template
//! recognises.

use std::io::Read as _;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::app::App;
use crate::vfs::{Vfs, VfsPath};
use crate::viewer::fileinfo::{FileFacts, FileInfo, HEAD_BYTES, describe};

/// How deep the answer's channel is.
///
/// One request is outstanding at a time - a second keystroke replaces the
/// first rather than queueing behind it - so this is never full. It is bounded
/// because every channel in the event loop is.
pub const FILE_INFO_CHANNEL_DEPTH: usize = 2;

/// What the keystroke knew, on its way to the event loop.
///
/// The facts come from the panel rather than from a second `stat`: the listing
/// has already formatted the size, the attributes and the date, and asking the
/// filesystem again could answer differently from what is on screen.
#[derive(Debug, Clone)]
pub struct FileInfoRequest {
    /// What to read.
    pub path: VfsPath,
    /// The name as the listing shows it.
    pub name: String,
    /// Size in bytes.
    pub size: u64,
    /// The attribute string, in the panel's own spelling.
    pub attrs: String,
    /// The modified date, in the panel's own spelling. Empty where unknown.
    pub modified: String,
    /// Directories are described without reading anything.
    pub is_dir: bool,
}

impl App {
    /// Queue the dialog for the file the keystroke was about.
    ///
    /// A second press replaces the first: the answer to the older one would
    /// open a dialog about a file the cursor has already left.
    pub fn request_file_info(&mut self, request: FileInfoRequest) {
        self.pending_file_info = Some(request);
    }

    /// Perform the queued read, off the event loop.
    pub fn service_file_info(&mut self, tx: &mpsc::Sender<FileInfo>) {
        let Some(request) = self.pending_file_info.take() else {
            return;
        };
        let vfs = Arc::clone(&self.vfs);
        let tx = tx.clone();
        tokio::task::spawn_blocking(move || {
            let head = read_head(vfs.as_ref(), &request.path, request.is_dir);
            let facts = FileFacts::new(&request.name, request.size, &request.attrs)
                .modified(&request.modified)
                .directory(request.is_dir);
            let _ = tx.blocking_send(describe(&facts, &head));
        });
    }
}

/// The front of the file, or nothing at all.
///
/// Bounded by [`HEAD_BYTES`] whatever the file's size claims, so a backend
/// that lies about a length cannot turn this into an unbounded read. A
/// directory is not opened: there is nothing in it to recognise.
fn read_head(vfs: &dyn Vfs, path: &VfsPath, is_dir: bool) -> Vec<u8> {
    if is_dir {
        return Vec::new();
    }
    let Ok(reader) = vfs.open_read(path) else {
        return Vec::new();
    };
    let mut head = Vec::new();
    // A short read is the answer for a short file, and a failed one is the
    // answer for a file that stopped being readable: both leave whatever
    // arrived, and fewer templates can claim it.
    let _ = reader.take(HEAD_BYTES as u64).read_to_end(&mut head);
    head
}
