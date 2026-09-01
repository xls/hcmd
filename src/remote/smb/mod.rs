//! SMB2/SMB3: a Windows share and a NAS box, behind the ordinary
//! [`RemoteTransport`] contract.
//!
//! `smb2` is pure Rust, has no C dependency and speaks SMB 2.0.2 through
//! 3.1.1 with signing and encryption. It is async and tokio-shaped, so this
//! backend has an actor exactly as SFTP does: [`task::run`] owns the session
//! and is the only code here that awaits a socket, and every method on this
//! type sends it a command and blocks on the reply.
//!
//! # A share is not a directory
//!
//! This is the one thing SMB has that neither SFTP nor FTP does, and the
//! decision is written here rather than left to each call site.
//!
//! ```text
//! smb://thorin@nas.local:445/Media/Photos/2024
//!                            ^^^^^ ^^^^^^^^^^^
//!                            share   path in the share
//! ```
//!
//! **A share is the first component of the connection's path, and nothing
//! else.** A [`crate::vfs::VfsPath`] on an SMB connection is one
//! `Remote(id)` segment whose text is `/Media/Photos/2024`, which is the
//! same single segment SFTP and FTP produce; there is no extra
//! [`crate::vfs::BackendKind`], no second segment, and no special case
//! anywhere above this module. Three things follow, and all three are what a
//! user expects:
//!
//! * `/` is the **server**, and listing it lists the shares. A connection
//!   whose line named no share opens there, which is the SMB answer to
//!   "wherever the server puts us".
//! * `..` out of a share's root lands on the server, because it is one path
//!   component up and nothing more.
//! * Two shares are two subtrees of one connection, so a copy between them is
//!   an ordinary copy and the panel needs no notion of a share at all.
//!
//! What does *not* follow is a rename across shares: SMB renames within a
//! tree connect, so [`SmbFs::rename`] refuses one across two, and the copy
//! engine's own fallback does the move instead.
//!
//! # Secrets
//!
//! A password reaches exactly one place in this module, the [`smb2::
//! ClientConfig`] built in [`connect::connect`], and is never held by
//! [`SmbFs`], never formatted into an error and never in a `Debug`. smb2
//! keeps its own copy for the life of the client so it can re-authenticate a
//! dropped session; that copy is inside the client and is not reachable from
//! anything this program renders.

pub mod actor;
pub mod connect;
pub mod files;
pub mod io;
pub mod ops;
pub mod task;

#[cfg(test)]
pub mod fake;

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::vfs::{Capabilities, Entry, ReadSeek};

use super::transport::RemoteTransport;
use super::{Protocol, Target};

use ops::SmbOps;

pub use actor::SmbActor;
pub use connect::connect;

/// One connected SMB server, behind [`RemoteTransport`].
pub struct SmbFs {
    /// Where it points. Secret-free, so it is safe in a header and an error.
    target: Target,
    /// The session underneath, or the fake the tests use.
    ops: Arc<dyn SmbOps>,
    /// The directory a panel opens on: the share and path the line named, or
    /// `/` - the list of shares - when it named none.
    start: String,
}

impl SmbFs {
    /// A backend over a live session.
    pub fn new(target: Target, ops: Arc<dyn SmbOps>, start: String) -> Arc<Self> {
        Arc::new(Self { target, ops, start })
    }

    /// The directory a panel opens on.
    ///
    /// Inherent rather than on [`RemoteTransport`], for the reason
    /// `FtpFs::start_dir` gives: it is answered once at connect time and never
    /// again.
    pub fn start_dir(&self) -> &str {
        &self.start
    }

    /// Where this connection points.
    pub fn target(&self) -> &Target {
        &self.target
    }

    /// The share and the path inside it, or `None` for the server itself.
    ///
    /// `/` is the server, `/Media` is a share's root - which is the share with
    /// an empty path, because that is how SMB addresses it - and
    /// `/Media/Photos` is a directory on it.
    fn split(path: &str) -> Option<(&str, &str)> {
        let rest = path.trim_start_matches('/');
        if rest.is_empty() {
            return None;
        }
        match rest.split_once('/') {
            None => Some((rest, "")),
            Some((share, inside)) => Some((share, inside.trim_end_matches('/'))),
        }
    }

    /// The share and a **non-empty** path inside it, for the operations that
    /// cannot act on a share or on the server.
    ///
    /// One refusal, phrased once: "a share is not a file" is the true answer
    /// to writing to `/Media`, and it is a better one than whatever a server
    /// says to a CREATE with an empty name.
    fn inside<'a>(&self, path: &'a str, what: &str) -> Result<(&'a str, &'a str)> {
        match Self::split(path) {
            None => Err(Error::InvalidPath(format!(
                "{}: {what} needs a share, and / is the server itself",
                self.target.authority()
            ))),
            Some((_, "")) => Err(Error::InvalidPath(format!(
                "{path}: that is a share, not a file"
            ))),
            Some(pair) => Ok(pair),
        }
    }

    /// The shares, as the rows a panel shows for `/`.
    ///
    /// A share is rendered as a directory because that is what entering one
    /// does. The server's comment is not shown: a panel row is a name, and a
    /// name that is not the one an operation addresses is a name that
    /// misleads.
    fn share_rows(&self) -> Result<Vec<Entry>> {
        Ok(self
            .ops
            .shares()?
            .into_iter()
            .map(|share| Entry::dir(share.name))
            .collect())
    }
}

impl RemoteTransport for SmbFs {
    fn protocol(&self) -> Protocol {
        Protocol::Smb
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::SMB
    }

    fn list(&self, dir: &str) -> Result<Vec<Entry>> {
        match Self::split(dir) {
            None => self.share_rows(),
            Some((share, inside)) => self.ops.list(share, inside),
        }
    }

    fn stat(&self, path: &str) -> Result<Entry> {
        match Self::split(path) {
            // The server itself. It is a directory and it has no name, which
            // is exactly what the root of every other backend answers.
            None => Ok(Entry::dir("/")),
            Some((share, inside)) => self.ops.stat(share, inside),
        }
    }

    /// SMB has reparse points; this backend does not read them.
    ///
    /// Reporting a link it cannot resolve would promise the copy engine an
    /// answer to `read_link` that it could not give, so no entry from this
    /// backend is ever [`crate::vfs::EntryKind::Symlink`] and this refuses.
    fn read_link(&self, _path: &str) -> Result<String> {
        Err(Error::Unsupported("reading a link over SMB"))
    }

    fn open_read(&self, path: &str) -> Result<Box<dyn std::io::Read + Send>> {
        let (share, inside) = self.inside(path, "reading a file")?;
        Ok(Box::new(self.ops.open_read(share, inside)?))
    }

    /// SMB2 READ carries an offset, so there is one reader and it seeks.
    fn open_seek(&self, path: &str) -> Result<Box<dyn ReadSeek + Send>> {
        let (share, inside) = self.inside(path, "reading a file")?;
        self.ops.open_read(share, inside)
    }

    fn open_write(&self, path: &str) -> Result<Box<dyn std::io::Write + Send>> {
        let (share, inside) = self.inside(path, "writing a file")?;
        self.ops.open_write(share, inside)
    }

    fn create_dir(&self, path: &str) -> Result<()> {
        let (share, inside) = self.inside(path, "creating a directory")?;
        self.ops.create_dir(share, inside)
    }

    fn remove_file(&self, path: &str) -> Result<()> {
        let (share, inside) = self.inside(path, "deleting a file")?;
        self.ops.remove_file(share, inside)
    }

    fn remove_dir(&self, path: &str) -> Result<()> {
        let (share, inside) = self.inside(path, "deleting a directory")?;
        self.ops.remove_dir(share, inside)
    }

    /// Rename, within one share.
    ///
    /// SMB renames inside a tree connect, so a rename that crosses two shares
    /// is refused here rather than half-attempted on the server. The move
    /// engine's own copy-then-delete fallback is what performs it.
    fn rename(&self, from: &str, to: &str) -> Result<()> {
        let (share, source) = self.inside(from, "renaming")?;
        let (target_share, dest) = self.inside(to, "renaming")?;
        if share != target_share {
            return Err(Error::Unsupported("renaming between two shares"));
        }
        self.ops.rename(share, source, dest)
    }

    fn is_live(&self) -> bool {
        self.ops.is_live()
    }

    fn close(&self) {
        self.ops.close();
    }
}

impl std::fmt::Debug for SmbFs {
    /// Target and start directory. No session, and there is no secret
    /// anywhere in reach of this type.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmbFs")
            .field("target", &self.target)
            .field("start", &self.start)
            .field("live", &self.is_live())
            .finish()
    }
}

#[cfg(test)]
#[path = "smb_tests.rs"]
mod tests;
