//! The crate-wide error type.
//!
//! Everything fallible in `holoscommander` returns [`Result<T>`]. There is no
//! `unwrap()` on anything that can fail at runtime (rule 5 is
//! about dependencies; this is the same instinct applied to control flow).

use std::fmt::Display;
use std::path::{Path, PathBuf};

/// The wording of a connection that dropped under a running job.
///
/// A constant rather than a literal inside the `#[error]` attribute because
/// two places have to agree on it: [`Error::ConnectionLost`]'s `Display`, and
/// the net in [`crate::ops::FailReason`] that has to recognise this event when
/// it arrives as text rather than as a variant. A copy of the sentence that
/// drifted from the original would silently disarm that net.
pub const CONNECTION_LOST_TEXT: &str = "the connection was lost";

/// The wording of a connection closed under a running job.
///
/// Shared for [`CONNECTION_LOST_TEXT`]'s reason. The sentence is the
/// contract's and does not change with the variant.
pub const CONNECTION_CLOSED_TEXT: &str =
    "that connection has been closed - Ctrl+R reconnects, Ctrl+F returns to local";

/// The single error type for the crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An I/O failure that knows which path it happened on.
    #[error("{path}: {source}")]
    Io {
        /// The path the operation was attempted on.
        path: PathBuf,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// An I/O failure with no path context.
    #[error("{0}")]
    Bare(#[from] std::io::Error),

    /// A configuration file could not be understood.
    #[error("{file}: {message}")]
    Config {
        /// The offending file.
        file: PathBuf,
        /// What went wrong, in user-facing terms.
        message: String,
    },

    /// A key-binding string in `keymap.toml` could not be parsed.
    #[error("bad key binding {binding:?}: {reason}")]
    Binding {
        /// The binding string as written by the user.
        binding: String,
        /// Why it was rejected.
        reason: String,
    },

    /// The backend does not support this operation at all (
    /// `Capabilities`). This is what the UI turns into "refused up front".
    #[error("{0} is not supported by this backend")]
    Unsupported(&'static str),

    /// Nothing at that path.
    #[error("not found: {0}")]
    NotFound(String),

    /// A `VfsPath` that cannot mean anything (empty segment stack, `..` past
    /// the root of a backend, and so on).
    #[error("invalid path: {0}")]
    InvalidPath(String),

    /// The user cancelled a running operation (the `Esc`).
    ///
    /// Not a failure: it is how a job reports that it stopped on request, and
    /// the difference matters because a cancelled copy must clean up its
    /// partial destination while a failed one has already done so.
    #[error("cancelled")]
    Cancelled,

    /// The connection to a remote host is gone.
    ///
    /// Its own variant rather than an [`Error::Msg`], because it is the one
    /// failure that must **stop a batch** instead of failing one file: every
    /// remaining file would fail identically, and two hundred identical lines
    /// in the failure summary say less than one.
    /// [`crate::ops::is_fatal`] is
    /// where the copy and delete loops ask.
    #[error("{0}: {CONNECTION_LOST_TEXT}")]
    ConnectionLost(String),

    /// A path naming a connection that is no longer registered.
    ///
    ///
    /// Its own variant rather than an [`Error::Msg`] for
    /// [`Error::ConnectionLost`]'s reason: a batch whose connection was closed
    /// under it - `Ctrl+F` on the panel, a tab closed, a job outliving the
    /// panel that started it - must **stop**, not fail two hundred files with
    /// this same sentence. The wording is the contract's and does not change
    /// with the variant.
    #[error("{0}: {CONNECTION_CLOSED_TEXT}")]
    ConnectionClosed(String),

    /// Anything else, already phrased for a human.
    #[error("{0}")]
    Msg(String),
}

impl Error {
    /// Attach a path to an [`std::io::Error`].
    pub fn io(path: impl AsRef<Path>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.as_ref().to_path_buf(),
            source,
        }
    }

    /// Build a [`Error::Config`].
    pub fn config(file: impl AsRef<Path>, message: impl Into<String>) -> Self {
        Self::Config {
            file: file.as_ref().to_path_buf(),
            message: message.into(),
        }
    }

    /// Build a [`Error::Msg`].
    pub fn msg(message: impl Into<String>) -> Self {
        Self::Msg(message.into())
    }

    /// Nothing at `path`.
    ///
    /// The half of the `path: why` convention that writes it; [`Error::subject`]
    /// is the half that reads it back. Going through here is what lets a caller
    /// that asked for one path decide whether the failure it got is about that
    /// path or about something underneath it.
    pub fn not_found(path: impl Display) -> Self {
        Self::NotFound(path.to_string())
    }

    /// `path` cannot mean anything, and why.
    ///
    /// Writes the `path: why` convention [`Error::subject`] reads back.
    pub fn invalid_path(path: impl Display, why: impl Display) -> Self {
        Self::InvalidPath(format!("{path}: {why}"))
    }

    /// The connection named by `authority` dropped under a running job.
    ///
    /// Writes the convention [`Error::subject`] reads back, and - the point of
    /// the variant - keeps the event classifiable by [`crate::ops::is_fatal`]
    /// instead of flattening it to a sentence.
    pub fn connection_lost(authority: impl Display) -> Self {
        Self::ConnectionLost(authority.to_string())
    }

    /// The connection named by `authority` was closed while a job still held it.
    pub fn connection_closed(authority: impl Display) -> Self {
        Self::ConnectionClosed(authority.to_string())
    }

    /// What this failure was about, in a form that can be compared against the
    /// path the caller asked for.
    ///
    /// The payload of [`Error::NotFound`], [`Error::InvalidPath`] and the two
    /// connection variants is written as `subject: why` by the constructors
    /// above, so the subject is everything up to the first `": "`. A payload
    /// with no `": "` is all subject, which is what the bare
    /// `Error::NotFound(path.to_string())` sites produce.
    ///
    /// This is a reader for a convention rather than a typed field, and it is
    /// worth saying why: the payload is a `String` at roughly 180 construction
    /// sites across the backends, and typing the field is a change to all of
    /// them. The constructors above are the migration path - code that builds
    /// its errors through them is comparable here by construction.
    pub fn subject(&self) -> Subject<'_> {
        match self {
            Self::Io { path, .. } | Self::Config { file: path, .. } => {
                path.to_str().map_or(Subject::Nothing, Subject::Path)
            }
            Self::NotFound(what) | Self::InvalidPath(what) => Subject::Path(head(what)),
            Self::ConnectionLost(authority) | Self::ConnectionClosed(authority) => {
                Subject::Connection(head(authority))
            }
            Self::Bare(_)
            | Self::Binding { .. }
            | Self::Unsupported(_)
            | Self::Cancelled
            | Self::Msg(_) => Subject::Nothing,
        }
    }

    /// Whether this failure is about exactly `what`, and not about something
    /// else that happened to fail during the same operation.
    ///
    /// A batch that asked for one path and got a [`Error::NotFound`] naming a
    /// different one is looking at a bug, not at a missing file, and this is
    /// how it can tell.
    pub fn is_about(&self, what: &impl Display) -> bool {
        let what = what.to_string();
        match self.subject() {
            Subject::Path(subject) | Subject::Connection(subject) => subject == what,
            Subject::Nothing => false,
        }
    }
}

/// Everything before the first `": "`, or the whole string when there is none.
fn head(text: &str) -> &str {
    match text.find(": ") {
        Some(cut) => text.get(..cut).unwrap_or(text),
        None => text,
    }
}

/// What a failure was about.
///
/// The difference between the two naming arms is not cosmetic: a path names
/// one item in a batch and a connection names every remaining item in it, and
/// that is the same distinction [`crate::ops::is_fatal`] turns into "fail this
/// file" versus "stop".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject<'a> {
    /// A path, as the backend that failed spells it.
    Path(&'a str),
    /// A connection, named by its authority.
    Connection(&'a str),
    /// The failure names nothing that can be compared against a path.
    Nothing,
}

/// The crate result alias.
pub type Result<T> = std::result::Result<T, Error>;
