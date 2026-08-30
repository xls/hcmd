//! What SMB has to be able to do for [`super::SmbFs`], and how a failure on
//! the wire becomes one of this program's errors.
//!
//! This is the boundary that makes the backend testable. Everything that is
//! *not* SMB - splitting a share off the front of a path, the server root that
//! lists shares, the `..` row, the refusals - lives in [`super::SmbFs`] above
//! this trait, and [`super::fake::FakeOps`] implements the trait in memory, so
//! the whole of that half is tested with no server anywhere. The one
//! implementation that touches a socket is [`super::actor::SmbActor`].
//!
//! Paths here are **inside a share** and are `/`-separated with no leading
//! slash: the share root is `""` and a file in it is `"notes.txt"`. That is
//! the shape smb2 takes, and converting once at this boundary keeps the
//! share-splitting in one place.

use crate::error::{Error, Result};
use crate::vfs::{Entry, EntryKind, ReadSeek};

/// One share on the server, as it named itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Share {
    /// The share name, which is the first component of every path on it.
    pub name: String,
    /// The server's own description, or empty. Never rendered as a path.
    pub comment: String,
}

/// The SMB operations [`super::SmbFs`] needs.
///
/// **Every method blocks. Call from the blocking pool only**, with the two
/// exceptions that say otherwise on themselves: [`SmbOps::is_live`] and
/// [`SmbOps::close`] read and set flags and never wait on a socket.
pub trait SmbOps: Send + Sync {
    /// Every disk share the server is willing to name.
    ///
    /// **Blocking.** Call from the blocking pool only.
    fn shares(&self) -> Result<Vec<Share>>;

    /// One directory inside a share, without `.` and `..`.
    ///
    /// **Blocking.** Call from the blocking pool only.
    fn list(&self, share: &str, dir: &str) -> Result<Vec<Entry>>;

    /// One path's metadata inside a share.
    ///
    /// **Blocking.** Call from the blocking pool only.
    fn stat(&self, share: &str, path: &str) -> Result<Entry>;

    /// A random-access reader over one file.
    ///
    /// SMB2 READ carries an offset, so there is one reader rather than a
    /// forward-only one and a seeking one.
    ///
    /// **Blocking.** Call from the blocking pool only.
    fn open_read(&self, share: &str, path: &str) -> Result<Box<dyn ReadSeek + Send>>;

    /// A writer whose `flush` is the commit.
    ///
    /// **Blocking.** Call from the blocking pool only.
    fn open_write(&self, share: &str, path: &str) -> Result<Box<dyn std::io::Write + Send>>;

    /// Create one directory.
    ///
    /// **Blocking.** Call from the blocking pool only.
    fn create_dir(&self, share: &str, path: &str) -> Result<()>;

    /// Remove one file.
    ///
    /// **Blocking.** Call from the blocking pool only.
    fn remove_file(&self, share: &str, path: &str) -> Result<()>;

    /// Remove one empty directory.
    ///
    /// **Blocking.** Call from the blocking pool only.
    fn remove_dir(&self, share: &str, path: &str) -> Result<()>;

    /// Rename, server side, within one share.
    ///
    /// **Blocking.** Call from the blocking pool only.
    fn rename(&self, share: &str, from: &str, to: &str) -> Result<()>;

    /// False once the connection is gone. **Never does I/O.**
    fn is_live(&self) -> bool;

    /// Stop the actor and drop the session. Idempotent. **Never does I/O.**
    fn close(&self);
}

/// Turn an smb2 failure into one of this program's errors.
///
/// Three of them are load-bearing rather than cosmetic:
///
/// * [`Error::NotFound`] is what a panel and a copy both branch on.
/// * [`Error::ConnectionLost`] is what [`crate::ops::is_fatal`] reads to
///   **stop a batch** rather than fail two hundred files with one sentence
///   each. Every way a session can end - a dropped socket, an expired
///   session, a revival that ran out of budget - maps to it.
/// * Everything else is an [`Error::Msg`] that already names the path and is
///   already phrased for a human.
///
/// `where` names the path the operation was attempted on, never a credential:
/// smb2's own messages carry an NTSTATUS and a command, and neither carries
/// what was sent to authenticate.
pub fn translate(err: &smb2::Error, authority: &str, where_: &str) -> Error {
    use smb2::ErrorKind;
    match err.kind() {
        ErrorKind::NotFound => Error::NotFound(where_.to_string()),
        ErrorKind::ConnectionLost | ErrorKind::SessionExpired => {
            Error::ConnectionLost(authority.to_string())
        }
        ErrorKind::AccessDenied => Error::Msg(format!("{where_}: permission denied")),
        ErrorKind::AuthRequired => {
            Error::Msg(format!("{authority}: the server rejected that login"))
        }
        ErrorKind::AlreadyExists => Error::Msg(format!("{where_}: already exists")),
        _ => Error::Msg(format!("{where_}: {err}")),
    }
}

/// Whether a failure means the session is over, so the backend can set its
/// disconnected flag at the one place every call already goes through.
pub fn is_lost(err: &Error) -> bool {
    matches!(err, Error::ConnectionLost(_))
}

/// One listing row, or `None` for the `.` and `..` the server sends.
///
/// The `..` row this program shows is synthetic and is added by
/// `RemoteFs::read_dir`, unconditionally and first, so the server's own is
/// dropped here rather than being shown twice.
pub fn entry_from_row(entry: &smb2::DirectoryEntry) -> Option<Entry> {
    if entry.name == "." || entry.name == ".." {
        return None;
    }
    let mut out = if entry.is_directory {
        Entry::dir(entry.name.clone())
    } else {
        Entry::file(entry.name.clone())
    };
    out.size = if entry.is_directory { 0 } else { entry.size };
    out.mtime = entry.modified.to_system_time();
    Some(out)
}

/// One `stat` row. The name is the path's last component, which is what every
/// caller of `Vfs::stat` renders.
pub fn entry_from_stat(path: &str, info: &smb2::FileInfo) -> Entry {
    let name = path.rsplit('/').next().unwrap_or(path);
    let mut out = if info.is_directory {
        Entry::dir(name)
    } else {
        Entry::file(name)
    };
    out.kind = if info.is_directory {
        EntryKind::Dir
    } else {
        EntryKind::File
    };
    out.size = if info.is_directory { 0 } else { info.size };
    out.mtime = info.modified.to_system_time();
    out
}
