//! What a job is asked to do: the sources, the destination, the options.
//!
//! A [`JobSpec`] is the whole request; [`JobOptions`] is what the dialogs
//! collect and the worker obeys, plus the requests that cannot be a path -
//! a URL to download, a device to send to. The conflict question a worker
//! asks mid-batch and the decisions it can get back live here too, because
//! a decision is part of what the job was told to do.

use std::time::SystemTime;

use crate::config::OpsConfig;
use crate::vfs::VfsPath;

use super::kind::JobKind;
use super::walk::WalkOptions;
use super::{localsend, resize};

/// What to do about a destination that already exists.
///
/// Every one of these has an "all" variant, which is [`Decision::apply_to_all`]
/// rather than a separate set of enum arms - the choice and its scope are
/// orthogonal, and folding them together doubles the enum for nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConflictChoice {
    /// Replace the destination.
    Overwrite,
    /// Leave the destination alone and move on.
    Skip,
    /// Write beside it under a free name.
    Rename,
    /// Append the source to the end of the destination.
    Append,
    /// Overwrite only when the source is newer than the destination.
    OverwriteIfNewer,
    /// Overwrite only when the two differ in size.
    OverwriteIfDifferentSize,
}

impl ConflictChoice {
    /// A stable string id.
    pub const fn id(&self) -> &'static str {
        match self {
            Self::Overwrite => "overwrite",
            Self::Skip => "skip",
            Self::Rename => "rename",
            Self::Append => "append",
            Self::OverwriteIfNewer => "overwrite_if_newer",
            Self::OverwriteIfDifferentSize => "overwrite_if_different_size",
        }
    }

    /// The label a conflict dialog's button carries.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Overwrite => "Overwrite",
            Self::Skip => "Skip",
            Self::Rename => "Rename",
            Self::Append => "Append",
            Self::OverwriteIfNewer => "If newer",
            Self::OverwriteIfDifferentSize => "If different size",
        }
    }

    /// Every choice, in the order a dialog should offer them.
    pub const ALL: &'static [Self] = &[
        Self::Overwrite,
        Self::Skip,
        Self::Rename,
        Self::Append,
        Self::OverwriteIfNewer,
        Self::OverwriteIfDifferentSize,
    ];
}

/// Everything the UI needs to describe one conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictRequest {
    /// The file being copied or moved.
    pub source: VfsPath,
    /// The existing file in the way.
    pub dest: VfsPath,
    /// Source size in bytes.
    pub source_size: u64,
    /// Destination size in bytes.
    pub dest_size: u64,
    /// Source mtime, `None` when the backend does not report one.
    pub source_mtime: Option<SystemTime>,
    /// Destination mtime.
    pub dest_mtime: Option<SystemTime>,
    /// True when both sides are directories, where only
    /// [`ConflictChoice::Skip`] and a merge (`Overwrite`) make sense.
    pub both_dirs: bool,
    /// True when the **destination** is a directory, whatever the source is.
    ///
    /// Separate from `both_dirs` because the asymmetric case is the dangerous
    /// one: a file arriving where a directory already stands must not be
    /// described to the user as a file-versus-file collision, and must not be
    /// answered by recursively removing that directory
    /// ([`conflict::Plan::Refuse`]).
    pub dest_is_dir: bool,
    /// The source is a stream that can be continued from where the
    /// destination stops - a download - so the dialog offers `Append` as
    /// *Resume*. False for a copy, where appending is appending.
    pub resumable: bool,
}

/// The UI's answer to a [`JobEvent::NeedsDecision`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Resolve this conflict.
    Conflict {
        /// What to do.
        choice: ConflictChoice,
        /// A name for [`ConflictChoice::Rename`]. `None` asks the worker to
        /// generate a free one - which is also what an "all" rename means,
        /// since one typed name cannot serve a whole batch.
        rename_to: Option<String>,
        /// "Decisions apply for the remainder of the batch."
        /// True installs `choice` as the standing policy.
        apply_to_all: bool,
    },
    /// Abandon the job. Equivalent to setting the [`CancelFlag`], and provided
    /// so a dialog that is already answering has one channel to answer on.
    Cancel,
}

/// What `Alt+F5`'s dialog decided, for a job that is a **pack**.
///
///
/// It rides on [`JobOptions`] rather than on [`JobKind`] because a pack is a
/// copy - or, with "move to archive", a move - as far as everything the user
/// sees is concerned: the same dialog, the same progress, the same summary.
/// What it changes is where the bytes go, which is [`crate::ops::pack`]'s
/// business and nobody else's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackInto {
    /// Which of the formats to write.
    pub format: crate::vfs::archive::format::FormatId,
    /// `0` stores, `9` is maximum; every format maps it onto its own scale.
    pub level: u8,
}

/// Everything the copy/move dialog collects and the worker obeys.
///
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobOptions {
    /// "Symlinks are copied as links by default;
    /// `ops.follow_symlinks` changes it."
    pub follow_symlinks: bool,
    /// Mode, mtime, and where permitted uid/gid and xattrs. See
    /// [`copy::preserve`] for what "where permitted" actually amounts to
    /// without a `libc` dependency.
    pub preserve_attrs: bool,
    /// Re-read and compare after writing (the Verify checkbox).
    pub verify: bool,
    /// "Only files of this type": a wildcard mask filtering what actually gets
    /// copied out of the selection. Empty or `*` means everything.
    pub file_mask: String,
    /// A conflict policy already chosen, so the batch runs without asking.
    /// `None` asks the UI on the first conflict.
    pub conflict: Option<ConflictChoice>,
    /// Set only by `Alt+F5`: this job writes a **new archive** rather than
    /// copying into an existing destination.
    pub pack: Option<PackInto>,
    /// How many bytes go in each part of a [`JobKind::Split`]. Zero everywhere
    /// else, and refused there.
    pub part_size: u64,
    /// Set only by `Shift+R`: what the resize dialog collected.
    ///
    /// It rides here rather than on [`JobSpec`] for the reason [`PackInto`]
    /// does: every kind of job that has ever needed its own settings has taken
    /// them from the dialog through these options, and one shape is easier to
    /// keep true than two.
    pub resize: Option<resize::ResizeSettings>,
    /// Set only by a [`JobKind::Download`]: the URL and whether to resume a
    /// partial file. Rides here for the same reason [`PackInto`] and
    /// [`JobOptions::resize`] do - the source is not a [`VfsPath`], so it cannot
    /// live in [`JobSpec::sources`].
    pub download: Option<DownloadRequest>,
    /// Set only by a [`JobKind::LocalSend`]: the device and the PIN. The
    /// files are the sources; this is what cannot be a path.
    pub localsend: Option<localsend::LocalSendRequest>,
}

/// What a [`JobKind::Download`] fetches, and how to treat a file already there.
///
/// The one reusable shape every way of starting a download produces - the
/// `Ctrl+D` prompt and the viewer's links alike - so the runner has one thing
/// to read and the callers have one thing to build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadRequest {
    /// Where to fetch from. Redirects are followed.
    pub url: String,
    /// Continue an existing partial file with a `Range` request rather than
    /// truncating it. The caller decides this - typically from the same
    /// resume-or-overwrite prompt a copy shows - so the runner does not have to.
    pub resume: bool,
}

impl Default for JobOptions {
    fn default() -> Self {
        Self {
            follow_symlinks: false,
            preserve_attrs: true,
            verify: false,
            file_mask: String::new(),
            conflict: None,
            pack: None,
            part_size: 0,
            resize: None,
            download: None,
            localsend: None,
        }
    }
}

impl JobOptions {
    /// The defaults from `[ops]` in `config.toml`.
    pub fn from_config(cfg: &OpsConfig) -> Self {
        Self {
            follow_symlinks: cfg.follow_symlinks,
            preserve_attrs: cfg.preserve_attrs,
            verify: false,
            file_mask: String::new(),
            conflict: if cfg.confirm_overwrite {
                None
            } else {
                Some(ConflictChoice::Overwrite)
            },
            pack: None,
            part_size: 0,
            resize: None,
            download: None,
            localsend: None,
        }
    }

    /// The [`WalkOptions`] these options imply, so a pre-flight walk and the
    /// copy that follows it agree about symlinks.
    pub const fn walk(&self) -> WalkOptions {
        WalkOptions {
            follow_symlinks: self.follow_symlinks,
        }
    }
}

/// One job, fully described. The dialog produces it; the worker consumes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSpec {
    /// What to do.
    pub kind: JobKind,
    /// What to do it to. Never empty except for [`JobKind::Mkdir`].
    pub sources: Vec<VfsPath>,
    /// Where to do it. The target *directory* for `Copy`/`Move`, the directory
    /// to create for `Mkdir`, and `None` for `Delete` and `Size`.
    pub dest: Option<VfsPath>,
    /// The dialog's answers.
    pub options: JobOptions,
    /// The new addresses, one per source, for [`JobKind::Rename`].
    /// **Empty for every other kind**, and an invariant
    /// checks it.
    pub targets: Vec<VfsPath>,
}

impl JobSpec {
    /// A job with default options.
    pub fn new(kind: JobKind, sources: Vec<VfsPath>, dest: Option<VfsPath>) -> Self {
        Self {
            kind,
            sources,
            dest,
            options: JobOptions::default(),
            targets: Vec::new(),
        }
    }

    /// A [`JobKind::Rename`] over pairs.
    ///
    /// The pairs are split into two positionally-matched vectors rather than
    /// carried as a `Vec<(VfsPath, VfsPath)>`, because `sources` is what every
    /// other part of the job machinery - the progress dialog, the failure
    /// summary, the retry - already reads.
    pub fn rename(pairs: Vec<(VfsPath, VfsPath)>) -> Self {
        let (sources, targets): (Vec<VfsPath>, Vec<VfsPath>) = pairs.into_iter().unzip();
        Self {
            kind: JobKind::Rename,
            sources,
            dest: None,
            options: JobOptions::default(),
            targets,
        }
    }

    /// A [`JobKind::Size`] walk over some paths.
    pub fn size(sources: Vec<VfsPath>) -> Self {
        Self::new(JobKind::Size, sources, None)
    }

    /// A [`JobKind::Compare`] over facing pairs.
    ///
    /// Split into two positionally-matched vectors exactly as
    /// [`JobSpec::rename`] is, and for the same reason: `sources` is what the
    /// progress dialog and the failure summary already read.
    pub fn compare(pairs: Vec<(VfsPath, VfsPath)>) -> Self {
        let (sources, targets): (Vec<VfsPath>, Vec<VfsPath>) = pairs.into_iter().unzip();
        Self {
            kind: JobKind::Compare,
            sources,
            dest: None,
            options: JobOptions::default(),
            targets,
        }
    }

    /// A [`JobKind::CompareFiles`] over exactly one facing pair.
    ///
    /// The same two positionally-matched vectors [`JobSpec::compare`] builds,
    /// with one entry each, so the runner they share needs no special case for
    /// the single pair.
    pub fn compare_files(a: VfsPath, b: VfsPath) -> Self {
        Self {
            kind: JobKind::CompareFiles,
            sources: vec![a],
            dest: None,
            options: JobOptions::default(),
            targets: vec![b],
        }
    }

    /// Set the options, by value, so a spec can be built in one expression.
    #[must_use]
    pub fn with_options(mut self, options: JobOptions) -> Self {
        self.options = options;
        self
    }
}

#[cfg(test)]
#[path = "spec_tests.rs"]
mod tests;
