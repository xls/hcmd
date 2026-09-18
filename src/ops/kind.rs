//! What a job is: its id, its kind, and what a dialog can ask done to it.
//!
//! The vocabulary the rest of the engine speaks. A [`JobKind`] is the one
//! place the kinds are enumerated, so `id`, `title` and the verb for a
//! summary are read off it in one match each rather than spelled out again
//! wherever a kind is named.

use std::fmt;

/// Identifies one job for its whole life, including in the background queue.
///
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(pub u64);

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// What a job dialog asks be done to its job.
///
/// Five verbs on a [`JobId`], each naming the `App` method that performs it.
/// Defined here beside [`JobId`] and [`Decision`] rather than among the
/// dialogs that answer with one: [`crate::dialog::DialogResult::Job`] carries
/// this value, and the framework hands a dialog a key and nothing else - it
/// may name what `ops` defines, and nothing any particular dialog does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobAction {
    /// `F2` on the progress dialog: send it to the background queue, keep it
    /// running. [`crate::app::App::background_job`].
    Background(JobId),
    /// `Esc` on the progress dialog: stop it.
    /// [`crate::app::App::cancel_job`].
    Cancel(JobId),
    /// `Enter` in the queue view: bring it back "exactly as it was".
    /// [`crate::app::App::foreground_job`].
    Foreground(JobId),
    /// Drop a finished job from the queue view.
    /// [`crate::app::App::forget_job`].
    Forget(JobId),
    /// the "option to retry" the failures of a finished job.
    Retry(JobId),
}

impl JobAction {
    /// Which job it is about.
    pub const fn id(&self) -> JobId {
        match self {
            Self::Background(id)
            | Self::Cancel(id)
            | Self::Foreground(id)
            | Self::Forget(id)
            | Self::Retry(id) => *id,
        }
    }
}

/// What a job does.
///
/// `Pack` and `Unpack` join this enum in v0.5; adding a variant
/// is a source-compatible change for everything in this crate that matches
/// exhaustively on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobKind {
    /// `F5`: copy the sources into `dest`.
    Copy,
    /// `F6`: rename or move the sources into `dest`.
    Move,
    /// `F8` / `Shift+F8`: delete the sources.
    Delete {
        /// True for `F8`, which moves to the XDG trash; false for `Shift+F8`,
        /// which unlinks.
        trash: bool,
    },
    /// `F7`: create `dest`.
    Mkdir,
    /// `Ctrl+L` and `Space`: walk the sources and report their size.
    /// Reads nothing but metadata and writes
    /// nothing at all.
    Size,
    /// `Ctrl+M`: rename each source to the target beside it.
    /// Moves no bytes, so its progress is files only.
    Rename,
    /// `Shift+F2` with `ops.compare_contents` on: read both sides of each
    /// facing pair and say which differ. Writes nothing.
    Compare,
    /// Comparing two named files byte for byte, for a verdict rather than for
    /// a mark. Shares [`crate::ops::compare`]'s reader with [`Self::Compare`];
    /// it is a separate kind because its answer is a sentence about one pair,
    /// not a set of names to mark.
    CompareFiles,
    /// Cutting one file into numbered parts.
    Split,
    /// Putting a numbered set back together.
    Merge,
    /// Hashing files and writing a sidecar, or reading one and checking what
    /// it names. [`crate::ops::checksum`] does both; the flag says which.
    Checksum {
        /// True to check an existing sidecar, false to write a new one.
        verify: bool,
    },
    /// `Shift+R`: decode each source, resample it and write it into `dest`
    /// under the new format and the new name.
    Resize,
    /// `Ctrl+D` and the viewer's links: fetch a URL into `dest`, streaming and
    /// resumable. Its source is the URL carried in [`JobOptions::download`], not
    /// a [`VfsPath`], so `sources` is empty like a [`JobKind::Mkdir`]'s.
    Download,
    /// `Alt+X`: send the sources to a LocalSend device named in
    /// [`JobOptions::localsend`]. Reads the sources, writes nothing.
    LocalSend,
}

impl JobKind {
    /// A stable string id, for state files and messages.
    pub const fn id(&self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Move => "move",
            Self::Delete { trash: true } => "trash",
            Self::Delete { trash: false } => "delete",
            Self::Mkdir => "mkdir",
            Self::Size => "size",
            Self::Rename => "rename",
            Self::Compare => "compare",
            Self::CompareFiles => "compare_files",
            Self::Split => "split",
            Self::Merge => "merge",
            Self::Checksum { verify: false } => "checksum",
            Self::Checksum { verify: true } => "verify",
            Self::Resize => "resize",
            Self::Download => "download",
            Self::LocalSend => "localsend",
        }
    }

    /// A verb for the progress dialog's title.
    pub const fn title(&self) -> &'static str {
        match self {
            Self::Copy => "Copying",
            Self::Move => "Moving",
            Self::Delete { trash: true } => "Moving to trash",
            Self::Delete { trash: false } => "Deleting",
            Self::Mkdir => "Creating directory",
            Self::Size => "Calculating size",
            Self::Rename => "Renaming",
            Self::Compare => "Comparing",
            Self::CompareFiles => "Comparing files",
            Self::Split => "Splitting",
            Self::Merge => "Merging",
            Self::Checksum { verify: false } => "Checksumming",
            Self::Checksum { verify: true } => "Verifying",
            Self::Resize => "Resizing",
            Self::Download => "Downloading",
            Self::LocalSend => "Sending to device",
        }
    }

    /// True for a job that changes the filesystem. A [`JobKind::Size`] does
    /// not, which is what lets it run without a confirmation of any kind, and
    /// neither does a [`JobKind::Compare`]: the contents comparison
    /// reads both sides and writes nothing at all.
    pub const fn is_destructive(&self) -> bool {
        !matches!(
            self,
            Self::Size | Self::Compare | Self::CompareFiles | Self::Checksum { verify: true }
        )
    }

    /// True for a job whose sources are paired positionally with
    /// [`JobSpec::targets`] rather than copied into one [`JobSpec::dest`].
    ///
    /// Two kinds: [`JobKind::Rename`]'s source and its new name,
    /// and [`JobKind::Compare`]'s two facing files.
    /// The invariant that every other kind leaves `targets`
    /// empty is checked in this module's tests.
    pub const fn is_paired(&self) -> bool {
        matches!(self, Self::Rename | Self::Compare | Self::CompareFiles)
    }

    /// True for a job whose failures can be re-run as a fresh job over the
    /// sources that failed (the "option to retry the failures").
    ///
    /// A paired job cannot: [`JobSpec::targets`] is positional, so a retry
    /// over a *subset* of the sources would have to carry exactly the targets
    /// of the sources it kept, and dropping them instead queues a job with no
    /// targets that renames nothing and reports a clean run. the design
    /// gives a multi-rename `Undo` and a result list rather than a retry, so
    /// there is nothing missing here to add later - the retry is the wrong
    /// verb for it.
    pub const fn is_retryable(&self) -> bool {
        !self.is_paired()
    }
}

impl fmt::Display for JobKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

#[cfg(test)]
#[path = "kind_tests.rs"]
mod tests;
