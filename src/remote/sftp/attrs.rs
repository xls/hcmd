//! SFTP wire attributes to [`Entry`], and the path arithmetic that goes with
//! it.
//!
//! Nothing here does I/O, which is the point: the mapping is the part of an
//! SFTP backend that can be tested with no server anywhere, and
//! the design asks for exactly that to be separated out and
//! tested rather than reported as working because it compiled.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::{FileType, StatusCode};

use crate::error::Error;
use crate::vfs::{Entry, EntryKind};

/// The largest payload one `SSH_FXP_READ` or `SSH_FXP_WRITE` asks for when the
/// server has not told us otherwise.
///
/// 32 KiB is what draft-ietf-secsh-filexfer-02 section 6.4 obliges every server
/// to accept, and it is what OpenSSH's own client uses by default. A server
/// that announces `limits@openssh.com` raises it (see
/// [`super::WireLimits`]), which is where the throughput the design asks for
/// comes from; this constant is only the floor for a server that announces
/// nothing.
pub(crate) const SAFE_PAYLOAD: usize = 32 * 1024;

/// Map one directory row or one `SSH_FXP_LSTAT` reply onto a panel row.
///
/// `permissions` is passed through whole rather than masked, because it is the
/// same `st_mode` `LocalFs` puts in [`Entry::mode`] (`src/vfs/local.rs`) and
/// the attribute column renders the two identically. A server that reports no
/// permissions at all leaves the field `0`, which is the documented "this
/// backend has no concept of them" value and renders as an empty cell.
///
/// `to_dir` is **not** resolved here: the listing does not know, and one
/// `SSH_FXP_STAT` per symbolic link is a round trip this function cannot make.
/// [`super::actor`] resolves them afterwards, pipelined, and rewrites the
/// kind; a link that could not be resolved stays `to_dir: false`, which is the
/// same answer `LocalFs` gives for a broken link.
pub(crate) fn entry_from(name: &str, attrs: &russh_sftp::protocol::FileAttributes) -> Entry {
    Entry {
        name: name.to_string(),
        kind: kind_of(attrs.file_type()),
        size: attrs.size.unwrap_or(0),
        mtime: attrs.mtime.map(unix_time),
        mode: attrs.permissions.unwrap_or(0),
        uid: attrs.uid.unwrap_or(0),
        gid: attrs.gid.unwrap_or(0),
        // A leading dot is the Unix convention and SFTP is a Unix protocol;
        // the same test `Entry::file` applies locally.
        is_hidden: name.starts_with('.'),
        is_parent: false,
        location: None,
        hit: None,
    }
}

/// SFTP's four file types onto [`EntryKind`]'s four.
///
/// `FileType` is `russh_sftp`'s enum and not this crate's, so the wildcard arm
/// is allowed here and is what keeps a new variant in a future release from
/// being a build break rather than an `Other` row.
pub(crate) fn kind_of(ty: FileType) -> EntryKind {
    match ty {
        FileType::Dir => EntryKind::Dir,
        FileType::File => EntryKind::File,
        FileType::Symlink => EntryKind::Symlink { to_dir: false },
        FileType::Other => EntryKind::Other,
    }
}

/// Seconds since the epoch, as SFTP version 3 reports them.
///
/// Version 3 has no sub-second field and no negative times, so this is the
/// whole of the conversion. A time that overflows `SystemTime` is impossible
/// from a `u32` and needs no fallback.
fn unix_time(secs: u32) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(u64::from(secs))
}

/// `dir` and `name` joined in the remote's namespace.
///
/// Always `/`-separated, never `std::path`: SFTP paths are the server's, and
/// `PathBuf` on Windows would helpfully insert a backslash.
///
pub(crate) fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        return name.to_string();
    }
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// True for a `SSH_FXP_NAME` row that is not a panel row at all.
///
/// Two reasons, and the second is a security rule rather than a cosmetic one:
///
/// * The `.` and `..` rows every server sends. The `..` row is synthesised by
///   `RemoteFs` from the path, the way every other backend does it,
///   so the server's own pair is dropped here
///   rather than being renamed or sorted around.
/// * Anything that is not a plain file name. Section 6.7 of
///   draft-ietf-secsh-filexfer-02 makes `filename` "a file name, not a path",
///   so a row carrying a separator, a `..` component or an absolute path is a
///   server contradicting the protocol - and it is exactly the row that would
///   make a recursive download write outside its destination, because
///   `ops::copy` joins the name onto the destination as written (the design
///   16.5, [`crate::vfs::is_plain_name`]).
pub(crate) fn is_not_a_row(name: &str) -> bool {
    name == "." || name == ".." || !crate::vfs::is_plain_name(name)
}

/// A remote path from a `&str`, refusing the ones that cannot mean anything.
///
/// An empty path is `/`: that is what a caller that has trimmed its way to
/// nothing means, and asking the server about `""` gets a different answer on
/// every server.
pub(crate) fn normalise(path: &str) -> String {
    if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    }
}

/// One `russh_sftp` failure, phrased for a status line and attached to a path.
///
/// [`StatusCode::NoSuchFile`] becomes [`Error::NotFound`] so that the callers
/// that already distinguish it - the conflict policy, `stat`-then-remove -
/// keep working across backends. A transport-level failure becomes
/// [`Error::ConnectionLost`] rather than a generic message, because
/// the design makes that variant the one thing that stops a
/// batch instead of failing one file.
pub(crate) fn map_error(path: &str, authority: &str, err: &SftpError) -> Error {
    match err {
        SftpError::Status(status) => match status.status_code {
            StatusCode::NoSuchFile => Error::NotFound(path.to_string()),
            StatusCode::Eof => Error::msg(format!("{path}: end of file")),
            _ => Error::msg(format!("{path}: {}", status_text(status))),
        },
        // A dead socket, a closed channel or a request that never came back
        // are all one thing to everything above here.
        SftpError::IO(_) | SftpError::Timeout | SftpError::UnexpectedPacket => {
            Error::ConnectionLost(authority.to_string())
        }
        SftpError::UnexpectedBehavior(text) => {
            if text.contains("session closed") {
                Error::ConnectionLost(authority.to_string())
            } else {
                Error::msg(format!("{path}: {text}"))
            }
        }
        SftpError::Limited(text) => Error::msg(format!("{path}: {text}")),
    }
}

/// The server's own words when it sent any, and the status name when it did
/// not.
///
/// Servers routinely send an empty `error_message`, and "`/srv/media`: " with
/// nothing after the colon is worse than no message at all.
fn status_text(status: &russh_sftp::protocol::Status) -> String {
    if status.error_message.trim().is_empty() {
        status.status_code.to_string()
    } else {
        status.error_message.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh_sftp::protocol::{FileAttributes, Status, StatusCode};

    fn attrs(perm: u32, size: u64) -> FileAttributes {
        FileAttributes {
            size: Some(size),
            uid: Some(1000),
            user: None,
            gid: Some(100),
            group: None,
            permissions: Some(perm),
            atime: Some(1_700_000_000),
            mtime: Some(1_700_000_000),
        }
    }

    #[test]
    fn a_regular_file_row_carries_size_mode_and_time() {
        let e = entry_from("notes.txt", &attrs(0o100_644, 4096));
        assert_eq!(e.name, "notes.txt");
        assert_eq!(e.kind, EntryKind::File);
        assert_eq!(e.size, 4096);
        assert_eq!(e.mode, 0o100_644);
        assert_eq!(e.uid, 1000);
        assert_eq!(e.gid, 100);
        assert!(!e.is_hidden);
        assert_eq!(
            e.mtime,
            Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
        );
    }

    #[test]
    fn a_directory_and_a_link_get_their_kinds() {
        assert_eq!(entry_from("src", &attrs(0o40_755, 0)).kind, EntryKind::Dir);
        assert_eq!(
            entry_from("link", &attrs(0o120_777, 0)).kind,
            EntryKind::Symlink { to_dir: false }
        );
        assert_eq!(
            entry_from("sock", &attrs(0o140_755, 0)).kind,
            EntryKind::Other
        );
    }

    #[test]
    fn a_dotfile_is_hidden_and_a_server_that_reports_nothing_leaves_zeroes() {
        let e = entry_from(".bashrc", &FileAttributes::empty());
        assert!(e.is_hidden);
        assert_eq!(e.mode, 0, "an empty attribute set is not a mode of 0o000");
        assert_eq!(e.size, 0);
        assert_eq!(e.mtime, None);
        assert_eq!(e.kind, EntryKind::Other);
    }

    #[test]
    fn the_mode_is_passed_through_whole_so_it_matches_the_local_backend() {
        // `LocalFs` stores `meta.mode()`, which includes the file-type bits.
        // A remote row has to render through the same column code.
        let e = entry_from("run.sh", &attrs(0o100_755, 10));
        assert!(e.is_executable(), "0o755 is executable on both backends");
    }

    #[test]
    fn join_never_doubles_or_drops_a_separator() {
        assert_eq!(join("/srv/media", "a.mkv"), "/srv/media/a.mkv");
        assert_eq!(join("/", "a.mkv"), "/a.mkv");
        assert_eq!(join("", "a.mkv"), "a.mkv");
        assert_eq!(join("/srv/media/", "a.mkv"), "/srv/media/a.mkv");
    }

    #[test]
    fn the_servers_own_dot_rows_are_dropped() {
        assert!(is_not_a_row("."));
        assert!(is_not_a_row(".."));
        assert!(!is_not_a_row("..."));
        assert!(!is_not_a_row(".bashrc"));
    }

    /// A `filename` is a file name and not a path
    /// (draft-ietf-secsh-filexfer-02 section 6.7). A server that answers
    /// otherwise is aiming at `ops::copy`'s `dst.join(&child.name)`: an
    /// absolute name discards the destination and a `..` name walks out of
    /// it, which is Zip Slip's remote spelling.
    #[test]
    fn a_listing_name_that_is_a_path_is_not_a_row() {
        assert!(is_not_a_row("../../../.bashrc"));
        assert!(is_not_a_row("/etc/cron.d/pwn"));
        assert!(is_not_a_row("sub/dir"));
        assert!(is_not_a_row(""));
        assert!(is_not_a_row("with\u{0}nul"));
        // Ordinary names, including the awkward ones, stay.
        assert!(!is_not_a_row("a file with spaces.txt"));
        assert!(!is_not_a_row("...."));
        assert!(!is_not_a_row("a-b_c.tar.gz"));
    }

    #[test]
    fn an_empty_path_is_the_root_and_not_the_servers_guess() {
        assert_eq!(normalise(""), "/");
        assert_eq!(normalise("/srv"), "/srv");
    }

    fn status(code: StatusCode, message: &str) -> SftpError {
        SftpError::Status(Status {
            id: 1,
            status_code: code,
            error_message: message.to_string(),
            language_tag: String::new(),
        })
    }

    #[test]
    fn a_missing_file_maps_to_not_found() {
        let err = map_error(
            "/srv/gone",
            "sftp://t@h:22",
            &status(StatusCode::NoSuchFile, ""),
        );
        assert!(matches!(err, Error::NotFound(ref p) if p == "/srv/gone"));
    }

    #[test]
    fn a_dead_socket_maps_to_connection_lost_so_a_batch_stops() {
        // the whole point of the
        // variant is that `ops::is_fatal` can see it.
        let err = map_error(
            "/srv/a",
            "sftp://t@h:22",
            &SftpError::IO("broken pipe".into()),
        );
        assert!(matches!(err, Error::ConnectionLost(ref a) if a == "sftp://t@h:22"));
        let err = map_error("/srv/a", "sftp://t@h:22", &SftpError::Timeout);
        assert!(matches!(err, Error::ConnectionLost(_)));
    }

    #[test]
    fn a_permission_failure_keeps_the_servers_words_and_never_invents_them() {
        let err = map_error(
            "/root",
            "sftp://t@h:22",
            &status(StatusCode::PermissionDenied, "Permission denied"),
        );
        assert_eq!(err.to_string(), "/root: Permission denied");
        // ...and when the server says nothing, the status name stands in.
        let err = map_error(
            "/root",
            "sftp://t@h:22",
            &status(StatusCode::PermissionDenied, "  "),
        );
        assert_eq!(err.to_string(), "/root: Permission denied");
    }
}
