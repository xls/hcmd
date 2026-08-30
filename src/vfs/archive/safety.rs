//! Member names, and the one rule that is not negotiable.
//!
//! > Never extract to a path that escapes the destination directory. Reject
//! > entries containing `..` or absolute paths - this is the Zip Slip class of
//! > bug and is a real vulnerability, not a theoretical one.
//!
//! Two things follow from that, and both are implemented here rather than in
//! any one format:
//!
//! 1. **A member name is validated once, at index time**, by
//!    [`normalize_member`]. A name that could escape never becomes a
//!    [`super::index::Member`], so it is never listed, never `stat`ed, never
//!    opened and never extracted. Rejecting at the door is what makes the
//!    guarantee hold for *every* format including the ones whose library
//!    already checks - the design applies to all eight, and "the library
//!    probably handles it" is not a verification.
//! 2. **A destination is composed once, at extraction time**, by
//!    [`dest_path`], which re-validates rather than trusting the index. The
//!    check is cheap and the failure mode it prevents is arbitrary file
//!    overwrite, so it is done twice on purpose.
//!
//! Symlink members get a third check, [`safe_link_target`]: a link that is
//! *inside* the destination but *points* outside it is the second-order form
//! of the same bug, and every archive format can express one.
//!
//! # The extraction path
//!
//! The rest of this module is the layer every format extracts through:
//! [`Extraction`] plans one member at a time, [`extract`] carries the plan out,
//! and nothing else in the crate composes a destination or writes a member to
//! disk. One audited path rather than eight, because the check repeated eight
//! times is the check forgotten once.
//!
//! It answers four things a name check alone cannot:
//!
//! * **the resolved question.** `a/evil -> /etc` followed by `a/evil/passwd`
//!   escapes a purely textual check, because the second name is spotless. Every
//!   destination goes through [`ensure_no_symlink_escape`] before it is
//!   written, directories are created component by component by
//!   [`create_dir_within`] rather than by a link-following `mkdir -p`, and
//!   files are opened `O_CREAT|O_EXCL` so a planted link is replaced rather
//!   than written through.
//! * **links, both kinds.** A tar carries symbolic *and* hard links; a hard
//!   link's target is a member path from the archive root and gets the same
//!   treatment the link's own name got.
//! * **bombs.** A declared size wildly out of proportion to the container is
//!   refused before a byte moves; output that runs past what the header
//!   declared is refused while it runs. Both caps are in [`ExtractLimits`] and
//!   the refusal says which one it was.
//! * **duplicates.** One archive may name the same member twice. The second is
//!   a *conflict* - the policy answers it - and never a silent
//!   overwrite.
//!
//! Every refusal names the entry and the reason, is counted, and does not stop
//! the batch: the per-file rule, applied to a container.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};

use super::format::{ArchiveFormat, MemberSource};
use super::index::{Member, MemberKind};

/// The longest member path this backend will accept, in bytes.
///
/// `PATH_MAX` on Linux. A longer name cannot be extracted anywhere, so
/// carrying it through the index would only defer the failure.
pub const MAX_MEMBER_PATH: usize = 4096;

/// The longest single component, in bytes. `NAME_MAX`.
pub const MAX_MEMBER_COMPONENT: usize = 255;

/// Why a member name was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsafe {
    /// Nothing left after normalisation.
    Empty,
    /// An embedded NUL - a name no filesystem call can carry, and a classic
    /// truncation trick.
    Nul,
    /// Rooted at `/`, or a Windows drive or UNC prefix.
    Absolute,
    /// Contains a `..` component. Not resolved, not "cleaned": refused.
    ParentEscape,
    /// Longer than [`MAX_MEMBER_PATH`], or a component longer than
    /// [`MAX_MEMBER_COMPONENT`].
    TooLong,
}

impl Unsafe {
    /// A phrase for an error message, already in user-facing terms.
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Empty => "an empty name",
            Self::Nul => "a NUL byte in the name",
            Self::Absolute => "an absolute path",
            Self::ParentEscape => "a `..` component",
            Self::TooLong => "a name that is too long",
        }
    }
}

/// Normalise an archive member name to this backend's canonical form, or
/// refuse it.
///
/// The canonical form is what the index, the panel and every `VfsPath` tail
/// inside an archive agree on:
///
/// * separators are `/`,
/// * no leading `/`, no trailing `/`,
/// * no `.` components, no empty components,
/// * **no `..` components at all** - not resolved, refused, per.
///
/// `backslash_separators` is true only for formats that really use `\` as a
/// separator in the container (RAR). Everywhere else a backslash is an
/// ordinary character in a Unix filename and rewriting it would rename the
/// user's file.
pub fn normalize_member(
    raw: &str,
    backslash_separators: bool,
) -> std::result::Result<String, Unsafe> {
    if raw.contains('\0') {
        return Err(Unsafe::Nul);
    }
    if raw.len() > MAX_MEMBER_PATH {
        return Err(Unsafe::TooLong);
    }

    let unified;
    let text = if backslash_separators {
        unified = raw.replace('\\', "/");
        unified.as_str()
    } else {
        raw
    };

    // Absolute, in any of the shapes an archive can carry one. The drive-letter
    // and UNC cases cannot escape on Linux - `C:\x` is a legal Unix filename -
    // but an archive that carries one is a Windows archive being unpacked with
    // a Windows path, and turning that into a *file called* `C:` silently is
    // worse than refusing it.
    if text.starts_with('/') || text.starts_with("\\\\") {
        return Err(Unsafe::Absolute);
    }
    let bytes = text.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Err(Unsafe::Absolute);
    }

    let mut out = String::with_capacity(text.len());
    for part in text.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return Err(Unsafe::ParentEscape);
        }
        if part.len() > MAX_MEMBER_COMPONENT {
            return Err(Unsafe::TooLong);
        }
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(part);
    }
    if out.is_empty() {
        return Err(Unsafe::Empty);
    }
    Ok(out)
}

/// The same rules applied to a [`crate::vfs::VfsPath`] tail.
///
/// Inside an archive a `VfsPath` tail is `/`-rooted (`/inner/b.txt`) because
/// that is what the panel and `VfsPath::join` produce; the index keys on the
/// rootless form. The archive root is the empty string, and it is the only
/// path for which [`normalize_member`]'s `Empty` refusal is an acceptable
/// answer - so it is handled here rather than there.
pub fn member_key(tail: &Path) -> Result<String> {
    let text = tail.to_string_lossy();
    let trimmed = text.trim_start_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    normalize_member(trimmed, false)
        .map_err(|why| Error::InvalidPath(format!("{text}: rejected - {}", why.reason())))
}

/// Is `path` inside `root`, compared component by component?
///
/// Lexical, because the destination usually does not exist yet and
/// `canonicalize` cannot answer a question about a file that has not been
/// created. The symlink half of the question is [`ensure_no_symlink_escape`].
pub fn is_within(root: &Path, path: &Path) -> bool {
    let mut theirs = root.components();
    let mut mine = path.components();
    loop {
        match theirs.next() {
            None => return true,
            Some(want) => match mine.next() {
                Some(got) if got == want => {}
                _ => return false,
            },
        }
    }
}

/// Compose the destination for one member under `dest_root`, refusing anything
/// that would land outside it.
///
/// Re-validates `member` rather than trusting that it came from the index. The
/// result is `dest_root` joined with the member's components and nothing else:
/// no component of the member can be `..`, `.`, empty, absolute or rooted, so
/// the join cannot walk upwards.
pub fn dest_path(dest_root: &Path, member: &str) -> Result<PathBuf> {
    let clean = normalize_member(member, false).map_err(|why| {
        Error::InvalidPath(format!(
            "{member}: refused - {} would escape {}",
            why.reason(),
            dest_root.display()
        ))
    })?;
    let mut out = dest_root.to_path_buf();
    for part in clean.split('/') {
        out.push(part);
    }
    // Belt and braces. `normalize_member` already makes this unreachable; if it
    // ever stops doing so, this is the check that still refuses.
    if !is_within(dest_root, &out) {
        return Err(Error::InvalidPath(format!(
            "{member}: refused - it would escape {}",
            dest_root.display()
        )));
    }
    Ok(out)
}

/// Refuse a symlink member whose *target* leaves the destination.
///
/// A link is written, not followed, so this is not about what exists now: it
/// is about what the next entry in the same archive can then write through.
/// `a/evil -> /etc` followed by `a/evil/passwd` is the whole attack, and every
/// format that can store a symlink can store that pair.
pub fn safe_link_target(link_member: &str, target: &str) -> Result<()> {
    if target.contains('\0') {
        return Err(Error::InvalidPath(format!(
            "{link_member}: refused - a NUL byte in the link target"
        )));
    }
    if target.starts_with('/') {
        return Err(Error::InvalidPath(format!(
            "{link_member} -> {target}: refused - an absolute link target"
        )));
    }
    // The target is resolved relative to the link's own directory, so a `..`
    // is only safe if the link's depth absorbs it. Counting is possible, but
    // the design says "reject entries containing `..`", and a link target is
    // an entry's content in exactly the sense that matters.
    if target.split('/').any(|part| part == "..") {
        return Err(Error::InvalidPath(format!(
            "{link_member} -> {target}: refused - a `..` component in the link target \
"
        )));
    }
    Ok(())
}

/// Refuse a destination whose existing prefix contains a symlink pointing out
/// of `dest_root`.
///
/// The lexical check in [`dest_path`] answers "does this path name a place
/// inside the destination"; this one answers "does that place *resolve* to
/// somewhere inside the destination", which is a different question the moment
/// a previous entry - or another process - has put a link in the way.
///
/// Walks the components of `path` under `dest_root` and stops at the first one
/// that does not exist: a path that is not there yet cannot be a link.
pub fn ensure_no_symlink_escape(dest_root: &Path, path: &Path) -> Result<()> {
    if !is_within(dest_root, path) {
        return Err(Error::InvalidPath(format!(
            "{}: refused - outside {}",
            path.display(),
            dest_root.display()
        )));
    }
    let rest = path.strip_prefix(dest_root).unwrap_or(path);
    let mut here = dest_root.to_path_buf();
    for part in rest.components() {
        match part {
            Component::Normal(name) => here.push(name),
            // `dest_path` cannot produce any of these, and a caller that hands
            // us one is refused rather than interpreted.
            _ => {
                return Err(Error::InvalidPath(format!(
                    "{}: refused - an unexpected path component",
                    path.display()
                )));
            }
        }
        let meta = match std::fs::symlink_metadata(&here) {
            Ok(meta) => meta,
            // Not there yet: nothing below it can exist either.
            Err(_) => return Ok(()),
        };
        if meta.file_type().is_symlink() {
            let resolved = std::fs::canonicalize(&here).map_err(|e| Error::io(&here, e))?;
            let root = std::fs::canonicalize(dest_root).map_err(|e| Error::io(dest_root, e))?;
            if !is_within(&root, &resolved) {
                return Err(Error::InvalidPath(format!(
                    "{}: refused - a symbolic link that leaves {}",
                    here.display(),
                    dest_root.display()
                )));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The extraction path every format goes through.
// ---------------------------------------------------------------------------

/// The default cap on one member's extracted size: 64 GiB.
///
/// Not a guess at what is reasonable - it is the size past which "this is a
/// bomb" is a better explanation than "this is a disk image". A real 70 GiB
/// member exists somewhere, and the answer for it is
/// [`ExtractLimits::max_member_bytes`], not silence.
pub const DEFAULT_MAX_MEMBER_BYTES: u64 = 64 * 1024 * 1024 * 1024;

/// The default cap on one whole extraction's output: 256 GiB.
///
/// The per-member cap alone does not stop a bomb built out of a million
/// members that are each individually plausible, so the budget is charged
/// across the batch as well.
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024 * 1024;

/// The default cap on declared size ÷ container size for one member: 1000:1.
///
/// `42.zip` is 42 KiB and declares petabytes: its ratio is in the millions.
/// Ordinary data does not reach 1000:1 - text manages five, an already
/// compressed payload manages one - and the two shapes that do (a file of
/// zeroes, a sparse image) are exactly the ones a bomb imitates, which is why
/// the ratio only bites above [`ExtractLimits::ratio_floor`].
pub const DEFAULT_MAX_RATIO: u64 = 1_000;

/// Below this declared size the ratio cap does not apply: 1 GiB.
///
/// A 2 MiB file of zeroes compressing to 2 KiB is a ratio of a thousand and is
/// nobody's attack. The floor is what keeps the ratio check from refusing it.
pub const DEFAULT_RATIO_FLOOR: u64 = 1024 * 1024 * 1024;

/// How many individual refusals one [`Extraction`] keeps the text of.
///
/// The *count* is exact however many there are; the transcript is bounded,
/// because a hostile archive can hold a million refusable names and a summary
/// nobody can read is its own kind of failure.
pub const MAX_RECORDED_REFUSALS: usize = 100;

/// What one extraction is allowed to produce (the bomb cases).
///
/// Configurable rather than constant, and deliberately **not** a `config.toml`
/// key: the `[archive]` table has three entries, and inventing a
/// fourth would make the generated reference config describe a file the spec
/// does not define - the same reasoning the design records for
/// the cache budget. A caller that needs different numbers passes different
/// numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractLimits {
    /// The most one member may produce.
    pub max_member_bytes: u64,
    /// The most one extraction may produce in total.
    pub max_total_bytes: u64,
    /// The most a member may declare per byte of container.
    pub max_ratio: u64,
    /// The declared size below which [`ExtractLimits::max_ratio`] is not
    /// applied at all.
    pub ratio_floor: u64,
}

impl Default for ExtractLimits {
    fn default() -> Self {
        Self {
            max_member_bytes: DEFAULT_MAX_MEMBER_BYTES,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_ratio: DEFAULT_MAX_RATIO,
            ratio_floor: DEFAULT_RATIO_FLOOR,
        }
    }
}

impl ExtractLimits {
    /// No caps at all.
    ///
    /// For a caller that has already decided - a test, or a user who answered
    /// a dialog saying they meant it. It is a named thing rather than four
    /// `u64::MAX`s at a call site so that "this extraction is uncapped" is
    /// greppable.
    pub const fn unbounded() -> Self {
        Self {
            max_member_bytes: u64::MAX,
            max_total_bytes: u64::MAX,
            max_ratio: u64::MAX,
            ratio_floor: u64::MAX,
        }
    }

    /// Check a member's *declared* size before a byte of it is extracted.
    ///
    /// `container_bytes` is the size of the archive file itself; `0` means it
    /// is not known, and the ratio test is then skipped rather than guessed at.
    pub fn check_declared(
        &self,
        declared: u64,
        container_bytes: u64,
    ) -> std::result::Result<(), Overrun> {
        if declared > self.max_member_bytes {
            return Err(Overrun::Declared {
                declared,
                cap: self.max_member_bytes,
            });
        }
        if container_bytes > 0 && declared >= self.ratio_floor {
            let ratio = declared / container_bytes.max(1);
            if ratio > self.max_ratio {
                return Err(Overrun::Ratio {
                    declared,
                    container: container_bytes,
                    ratio,
                    cap: self.max_ratio,
                });
            }
        }
        Ok(())
    }
}

/// Which cap an entry ran into, with the numbers.
///
/// Four arms rather than one message, because the summary is only
/// useful if it says *why*: "the header lied" and "this archive is a bomb" call
/// for different reactions from the person reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overrun {
    /// The declared size alone is past [`ExtractLimits::max_member_bytes`].
    Declared {
        /// What the header claimed.
        declared: u64,
        /// The cap it passed.
        cap: u64,
    },
    /// The declared size is out of all proportion to the container's own size.
    Ratio {
        /// What the header claimed.
        declared: u64,
        /// How big the whole archive is.
        container: u64,
        /// `declared / container`.
        ratio: u64,
        /// The cap it passed.
        cap: u64,
    },
    /// The entry produced more bytes than its own header declared. Caught
    /// while extracting, which is the only place it can be caught.
    BeyondDeclared {
        /// What the header claimed.
        declared: u64,
        /// How much had arrived when the cap tripped.
        seen: u64,
    },
    /// The bytes actually produced passed the per-entry cap.
    ///
    /// Only reachable for a format that could not state a size in advance: a
    /// declared size is checked before a byte moves, and a member that then
    /// exceeds it trips [`Overrun::BeyondDeclared`] first.
    Produced {
        /// How much arrived.
        produced: u64,
        /// The cap it passed.
        cap: u64,
    },
    /// The whole extraction has produced more than its budget.
    Budget {
        /// How much this extraction has produced.
        total: u64,
        /// The cap it passed.
        cap: u64,
    },
}

impl Overrun {
    /// The phrase for a refusal message, numbers included.
    pub fn reason(&self) -> String {
        match *self {
            Self::Declared { declared, cap } => format!(
                "refused - it declares {}, past the {} this backend extracts in one entry",
                human(declared),
                human(cap)
            ),
            Self::Ratio {
                declared,
                container,
                ratio,
                cap,
            } => format!(
                "refused - it declares {} out of a {} archive, {ratio}:1 against a {cap}:1 cap \
                 (a decompression bomb)",
                human(declared),
                human(container)
            ),
            Self::BeyondDeclared { declared, seen } => format!(
                "refused - it has produced {} against the {} its header declared (a lying size)",
                human(seen),
                human(declared)
            ),
            Self::Produced { produced, cap } => format!(
                "refused - it has produced {}, past the {} cap for one entry",
                human(produced),
                human(cap)
            ),
            Self::Budget { total, cap } => format!(
                "refused - this extraction has produced {}, past its {} budget",
                human(total),
                human(cap)
            ),
        }
    }
}

impl std::fmt::Display for Overrun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason())
    }
}

/// One entry that was not extracted, and why.
///
/// > Errors never abort the whole batch silently: collect per-file failures and
/// > show a summary at the end.
///
/// The entry is always named. A user who cannot tell which of two thousand
/// files did not appear, or why, has been failed as surely as one whose machine
/// was compromised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The member, exactly as the archive named it.
    pub member: String,
    /// Why, already phrased for a human and starting with the verb.
    pub reason: String,
}

impl Refusal {
    /// A refusal of `member` for `reason`.
    pub fn new(member: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            member: member.into(),
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.member, self.reason)
    }
}

impl From<Refusal> for Error {
    fn from(refusal: Refusal) -> Self {
        Self::InvalidPath(refusal.to_string())
    }
}

/// What extracting one member actually does, once it has been allowed to.
///
/// Produced only by [`Extraction::plan`], so there is no way to reach a
/// destination that has not been through the checks: the paths in here are
/// already composed, already inside the destination, and already known not to
/// resolve through a symbolic link that leaves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Create this directory.
    ///
    /// Its mode is deliberately **not** applied here. A member of mode `0o555`
    /// applied before the entries below it are written would lock the
    /// extraction out of its own destination, and applying it afterwards means
    /// knowing when the batch ended - which is the caller's knowledge, not this
    /// layer's.
    Dir {
        /// Where.
        dest: PathBuf,
    },
    /// Write this file.
    File {
        /// Where.
        dest: PathBuf,
        /// What the header claims it will hold.
        declared: u64,
        /// The mode to apply afterwards, already sanitised by [`safe_mode`].
        mode: Option<u32>,
    },
    /// Create a symbolic link.
    Symlink {
        /// Where.
        dest: PathBuf,
        /// What it points at - relative, and known to stay inside the
        /// destination.
        target: String,
    },
    /// Hard-link `dest` to another member that has already been extracted.
    Hardlink {
        /// Where.
        dest: PathBuf,
        /// The member it links to, as a path under the destination root.
        source: PathBuf,
    },
}

impl Action {
    /// Where this action writes.
    pub fn dest(&self) -> &Path {
        match self {
            Self::Dir { dest }
            | Self::File { dest, .. }
            | Self::Symlink { dest, .. }
            | Self::Hardlink { dest, .. } => dest,
        }
    }
}

/// An [`Action`] plus what the caller still has to decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    /// What to do.
    pub action: Action,
    /// True when this extraction has already seen this exact member path.
    ///
    /// An archive may carry the same name twice; the second one is a
    /// **conflict**, to be put through the policy - overwrite /
    /// skip / rename / append, with its "all" variants - exactly like an
    /// existing file on disk. It is never a silent overwrite, because "the last
    /// entry wins" is a decision the user is entitled to make.
    pub duplicate: bool,
}

/// One extraction of one archive: the audited path, and the books it keeps.
///
/// Every format extracts through this. It is a single object rather than eight
/// copies of the same checks because the check that is written eight times is
/// the check that is forgotten once, and the thing it prevents is arbitrary
/// file overwrite.
///
/// The order of [`Extraction::plan`] is the whole of it:
///
/// 1. the name is re-normalised - `..`, absolute, drive-lettered, empty, `.`,
///    NUL-bearing and over-long names are **refused, not repaired**;
/// 2. the destination is composed by [`dest_path`] and by nothing else;
/// 3. the composed path is checked for a symbolic link in its prefix
///    ([`ensure_no_symlink_escape`]) - the second question, the one a purely
///    textual check cannot answer, and the one a real archive exploits by
///    planting `a/evil -> /etc` and then writing `a/evil/passwd`;
/// 4. a link's *target* is checked on the same terms, symbolic and hard alike;
/// 5. the declared size is checked against the bomb caps;
/// 6. a repeat of a name already seen is flagged as a conflict, not applied.
///
/// A refusal never stops the batch: it is recorded, named, counted, and the
/// next member is planned.
#[derive(Debug)]
pub struct Extraction {
    root: PathBuf,
    container_bytes: u64,
    limits: ExtractLimits,
    seen: HashSet<String>,
    /// Shared with every [`Guard`] this extraction hands out, so the budget is
    /// charged across members and across threads.
    total: Arc<AtomicU64>,
    refusals: Vec<Refusal>,
    refused: usize,
    accepted: usize,
}

impl Extraction {
    /// An extraction into `dest_root` out of a container of `container_bytes`,
    /// with the default caps.
    ///
    /// `container_bytes` may be `0` when the size is genuinely unknown; the
    /// ratio check is then skipped rather than computed from a made-up number.
    pub fn new(dest_root: impl Into<PathBuf>, container_bytes: u64) -> Self {
        Self::with_limits(dest_root, container_bytes, ExtractLimits::default())
    }

    /// [`Extraction::new`] with the caps given explicitly.
    pub fn with_limits(
        dest_root: impl Into<PathBuf>,
        container_bytes: u64,
        limits: ExtractLimits,
    ) -> Self {
        Self {
            root: dest_root.into(),
            container_bytes,
            limits,
            seen: HashSet::new(),
            total: Arc::new(AtomicU64::new(0)),
            refusals: Vec::new(),
            refused: 0,
            accepted: 0,
        }
    }

    /// The destination directory everything lands under.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The caps in force.
    pub fn limits(&self) -> ExtractLimits {
        self.limits
    }

    /// How many entries have been planned successfully.
    pub fn accepted(&self) -> usize {
        self.accepted
    }

    /// How many bytes this extraction has produced so far.
    pub fn bytes(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// Every refusal whose text was kept, oldest first.
    pub fn refusals(&self) -> &[Refusal] {
        &self.refusals
    }

    /// How many entries were refused in total, kept or not.
    pub fn refused(&self) -> usize {
        self.refused
    }

    /// Record a refusal the caller decided on itself - an I/O failure on one
    /// member, say - so it reaches the same summary as the ones decided here.
    pub fn refuse(&mut self, member: impl Into<String>, reason: impl Into<String>) -> Refusal {
        let refusal = Refusal::new(member, reason);
        self.record(refusal.clone());
        refusal
    }

    fn record(&mut self, refusal: Refusal) {
        self.refused = self.refused.saturating_add(1);
        if self.refusals.len() < MAX_RECORDED_REFUSALS {
            self.refusals.push(refusal);
        }
    }

    /// the end-of-batch summary, or `None` when nothing was
    /// refused.
    pub fn summary(&self) -> Option<String> {
        if self.refused == 0 {
            return None;
        }
        let mut text = if self.refused == 1 {
            "1 entry was not extracted:".to_string()
        } else {
            format!("{} entries were not extracted:", self.refused)
        };
        for refusal in self.refusals.iter().take(8) {
            text.push_str("\n  ");
            text.push_str(&refusal.to_string());
        }
        let shown = self.refusals.len().min(8);
        if self.refused > shown {
            let more = self.refused.saturating_sub(shown);
            text.push_str(&format!("\n  … and {more} more"));
        }
        Some(text)
    }

    /// A [`Guard`] for one member's bytes, charged against this extraction's
    /// budget.
    ///
    /// `declared` is `None` only for a format that genuinely cannot state a
    /// size before it reads; the absolute caps still apply, and only the
    /// "produced more than it declared" check is skipped.
    pub fn guard(&self, member: impl Into<String>, declared: Option<u64>) -> Guard {
        Guard {
            member: member.into(),
            declared,
            member_cap: self.limits.max_member_bytes,
            total_cap: self.limits.max_total_bytes,
            written: 0,
            total: Arc::clone(&self.total),
        }
    }

    /// Decide what, if anything, extracting `member` into this destination
    /// means.
    ///
    /// `Err` is a refusal that has **already been recorded**: the caller
    /// reports it and moves to the next member, and cannot
    /// forget to count it.
    pub fn plan(&mut self, member: &Member) -> std::result::Result<Planned, Refusal> {
        let dest = match dest_path(&self.root, &member.path) {
            Ok(dest) => dest,
            Err(err) => return Err(self.refuse_member(&member.path, err.to_string())),
        };
        // The resolved question, not the textual one: an earlier entry may have
        // planted a link across the path we are about to write through.
        //
        // For everything but a directory it is the *prefix* that is checked,
        // not the leaf: the leaf is never followed - it is opened
        // `O_CREAT|O_EXCL` or unlinked first - so a link sitting there is a
        // conflict for the policy, not an escape. A directory is
        // checked whole, because creating one at a link's name would mean
        // writing everything below it through the link.
        let prefix = if matches!(member.kind, MemberKind::Dir) {
            dest.clone()
        } else {
            dest.parent().unwrap_or(&self.root).to_path_buf()
        };
        if let Err(err) = ensure_no_symlink_escape(&self.root, &prefix) {
            return Err(self.refuse_member(&member.path, err.to_string()));
        }

        let action = match &member.kind {
            MemberKind::Dir => Action::Dir { dest },
            MemberKind::File => {
                if let Err(over) = self
                    .limits
                    .check_declared(member.size, self.container_bytes)
                {
                    return Err(self.refuse_member(&member.path, over.reason()));
                }
                Action::File {
                    dest,
                    declared: member.size,
                    mode: safe_mode(member.mode),
                }
            }
            MemberKind::Symlink(target) => {
                if target.is_empty() {
                    return Err(self.refuse_member(
                        &member.path,
                        "refused - the archive does not record where this link points",
                    ));
                }
                if let Err(err) = safe_link_target(&member.path, target) {
                    return Err(self.refuse_member(&member.path, err.to_string()));
                }
                // And where it lands, resolved against the link's own
                // directory. `safe_link_target` has already refused every `..`,
                // so this cannot climb; it is here because the composed path is
                // what the next entry will write through.
                let resolved = join_member(member.parent(), target);
                match normalize_member(&resolved, false) {
                    Ok(inside) => {
                        let landing = match dest_path(&self.root, &inside) {
                            Ok(landing) => landing,
                            Err(err) => {
                                return Err(self.refuse_member(&member.path, err.to_string()));
                            }
                        };
                        if !is_within(&self.root, &landing) {
                            return Err(self.refuse_member(
                                &member.path,
                                format!(
                                    "refused - it points at {target}, outside {}",
                                    self.root.display()
                                ),
                            ));
                        }
                    }
                    Err(why) => {
                        return Err(self.refuse_member(
                            &member.path,
                            format!("refused - its target {target} has {}", why.reason()),
                        ));
                    }
                }
                Action::Symlink {
                    dest,
                    target: target.clone(),
                }
            }
            MemberKind::Hardlink(target) => {
                // A hard link's target is another member of the same archive,
                // named from the archive root - not from the link's directory.
                // It gets the same treatment the link's own name got.
                let source = match normalize_member(target, false) {
                    Ok(clean) => match dest_path(&self.root, &clean) {
                        Ok(source) => source,
                        Err(err) => return Err(self.refuse_member(&member.path, err.to_string())),
                    },
                    Err(why) => {
                        return Err(self.refuse_member(
                            &member.path,
                            format!(
                                "refused - it hard-links to {target}, which has {} \
",
                                why.reason()
                            ),
                        ));
                    }
                };
                if let Err(err) = ensure_no_symlink_escape(&self.root, &source) {
                    return Err(self.refuse_member(&member.path, err.to_string()));
                }
                Action::Hardlink { dest, source }
            }
            MemberKind::Other => {
                return Err(self.refuse_member(
                    &member.path,
                    "refused - a device node, fifo or socket is not extracted from an archive",
                ));
            }
        };

        let duplicate =
            !self.seen.insert(member.path.clone()) && !matches!(member.kind, MemberKind::Dir);
        self.accepted = self.accepted.saturating_add(1);
        Ok(Planned { action, duplicate })
    }

    /// Record and return a refusal for `member`.
    fn refuse_member(&mut self, member: &str, reason: impl Into<String>) -> Refusal {
        let refusal = Refusal::new(member, reason);
        self.record(refusal.clone());
        refusal
    }
}

/// Join a member's parent directory with a relative link target.
fn join_member(parent: &str, target: &str) -> String {
    if parent.is_empty() {
        target.to_string()
    } else {
        format!("{parent}/{target}")
    }
}

/// The bytes of one member, counted against the caps as they arrive.
///
/// The declared size is a *claim*; this is where the claim meets the stream,
/// and it is the half of the bomb defence that a check on the header alone
/// cannot provide.
#[derive(Debug)]
pub struct Guard {
    member: String,
    declared: Option<u64>,
    member_cap: u64,
    total_cap: u64,
    written: u64,
    total: Arc<AtomicU64>,
}

impl Guard {
    /// A guard for one member read on its own, outside a planned extraction.
    ///
    /// [`super::ArchiveFs::open_read`] is the single funnel every byte of every
    /// member leaves an archive through - `ops`' copy for `F5` and `Alt+F6`,
    /// the `F3` viewer and the session cache all arrive there - and it has no
    /// destination directory, so there is no [`Extraction`] to charge against.
    /// The caps that do not need one still apply, and they are the exact half
    /// of the bomb defence rather than the heuristic half:
    ///
    /// * what the member's own header declared, so a member claiming a
    ///   kilobyte and producing a petabyte is stopped at the kilobyte;
    /// * [`ExtractLimits::max_member_bytes`], the absolute ceiling.
    ///
    /// [`ExtractLimits::max_ratio`] is deliberately **not** applied here.
    /// It is a planning heuristic about a whole extraction, and on this path it
    /// would refuse an honest member - a 2 GB log that gzip shrank to a
    /// megabyte is a ratio of 2000 and a perfectly ordinary thing to copy out
    /// of an archive. The checks above cannot refuse a member whose header
    /// told the truth, which is what a file manager needs.
    pub fn for_member(
        member: impl Into<String>,
        declared: Option<u64>,
        limits: ExtractLimits,
    ) -> Self {
        Self {
            member: member.into(),
            declared,
            member_cap: limits.max_member_bytes,
            // No batch, so no batch budget: the two caps above are the whole
            // of it, and `u64::MAX` says that rather than leaving a second
            // per-member ceiling to be mistaken for one.
            total_cap: u64::MAX,
            written: 0,
            total: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Charge `n` more bytes, refusing when a cap is passed and saying which.
    pub fn accept(&mut self, n: u64) -> Result<()> {
        let written = self.written.saturating_add(n);
        if let Some(declared) = self.declared
            && written > declared
        {
            return Err(self.overrun(Overrun::BeyondDeclared {
                declared,
                seen: written,
            }));
        }
        if written > self.member_cap {
            return Err(self.overrun(Overrun::Produced {
                produced: written,
                cap: self.member_cap,
            }));
        }
        let total = self.total.fetch_add(n, Ordering::Relaxed).saturating_add(n);
        if total > self.total_cap {
            return Err(self.overrun(Overrun::Budget {
                total,
                cap: self.total_cap,
            }));
        }
        self.written = written;
        Ok(())
    }

    /// How many bytes have been accepted.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// What the header claimed, if it claimed anything.
    pub fn declared(&self) -> Option<u64> {
        self.declared
    }

    /// The member this guard is for.
    pub fn member(&self) -> &str {
        &self.member
    }

    fn overrun(&self, over: Overrun) -> Error {
        Error::msg(format!("{}: {}", self.member, over.reason()))
    }
}

/// A [`Write`] that cannot produce more than its [`Guard`] allows.
///
/// Wrapped around whatever the caller is writing into - a file here, a copy
/// loop's destination in `ops` - so that "the output exceeded the declared
/// size" is caught at the 64 KiB granularity of the copy rather than after the
/// disk is full.
#[derive(Debug)]
pub struct GuardedWriter<W: Write> {
    out: W,
    guard: Guard,
}

impl<W: Write> GuardedWriter<W> {
    /// Guard `out` with `guard`.
    pub fn new(out: W, guard: Guard) -> Self {
        Self { out, guard }
    }

    /// The guard, for the byte counts.
    pub fn guard(&self) -> &Guard {
        &self.guard
    }

    /// Take the writer and the guard back.
    pub fn into_inner(self) -> (W, Guard) {
        (self.out, self.guard)
    }
}

impl<W: Write> Write for GuardedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Charged *before* the write, so the byte past the cap never reaches
        // the filesystem.
        self.guard
            .accept(buf.len() as u64)
            .map_err(std::io::Error::other)?;
        self.out.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

/// `PATH_MAX`: the longest link target that can name anything.
///
/// A target longer than a path can be is a container lying about what it
/// holds, and saying so costs one page rather than whatever the header
/// claimed.
pub const MAX_LINK_TARGET_BYTES: usize = 4096;

/// `O_NOFOLLOW`, so a file that turned into a symbolic link since it was
/// written is not read *through*.
///
/// Written out rather than taken from `libc`, which is not one of the crates.
/// It is a Linux ABI constant: `asm-generic/fcntl.h` has fixed it at
/// `0o400000` since the flag existed, and it is part of the syscall interface
/// rather than of any library.
pub const O_NOFOLLOW: i32 = 0o400_000;

/// Open `path` for reading, refusing to follow a symbolic link at the last
/// component.
///
/// The session cache holds files this program extracted with somebody else's
/// library. `unrar`, given a member whose attributes say `S_IFLNK`, creates a
/// **real symbolic link** at the path it was handed - and an ordinary
/// `File::open` of that path then reads whatever the link names, which is how
/// an archive that contains no such file at all gets one copied out of the
/// machine and into the unpack directory. `O_NOFOLLOW` is the answer that does
/// not depend on having checked first.
pub fn open_no_follow(path: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW)
        .open(path)
        .map_err(|e| {
            if e.raw_os_error() == Some(40) {
                // ELOOP: the last component is a symbolic link.
                Error::InvalidPath(format!(
                    "{}: refused - a symbolic link where a file was extracted \
",
                    path.display()
                ))
            } else {
                Error::io(path, e)
            }
        })
}

/// The mode to apply to an extracted file, or `None` when the archive recorded
/// none.
///
/// **setuid, setgid and the sticky bit are dropped.** They are the second
/// oldest trick after Zip Slip: an archive that unpacks a setuid-root shell is
/// a machine given away, and no ordinary unpack needs them. Whatever else the
/// archive asked for is honoured, because the design asks for mode to be
/// preserved.
pub fn safe_mode(mode: u32) -> Option<u32> {
    let bits = mode & 0o7777;
    if bits == 0 {
        return None;
    }
    // One rule, one implementation: the copy engine writes an extracted file's
    // mode through the same function (`crate::vfs::untrusted_mode`), because a
    // check that is written twice is the check that is fixed once.
    Some(crate::vfs::untrusted_mode(bits))
}

/// Create `dir` and every missing parent, refusing to walk through a symbolic
/// link.
///
/// `std::fs::create_dir_all` follows links, which is precisely the step the
/// two-phase Zip Slip relies on: entry one plants `a -> /etc`, entry two asks
/// for `a/b/` and a following `mkdir -p` obliges. This walks the components
/// itself and refuses at the first one that is a link, a file, or anything else
/// that is not already a real directory.
pub fn create_dir_within(dest_root: &Path, dir: &Path) -> Result<()> {
    if !is_within(dest_root, dir) {
        return Err(Error::InvalidPath(format!(
            "{}: refused - outside {}",
            dir.display(),
            dest_root.display()
        )));
    }
    if !dest_root.is_dir() {
        std::fs::create_dir_all(dest_root).map_err(|e| Error::io(dest_root, e))?;
    }
    let rest = dir.strip_prefix(dest_root).unwrap_or(Path::new(""));
    let mut here = dest_root.to_path_buf();
    for part in rest.components() {
        let Component::Normal(name) = part else {
            return Err(Error::InvalidPath(format!(
                "{}: refused - an unexpected path component",
                dir.display()
            )));
        };
        here.push(name);
        match std::fs::symlink_metadata(&here) {
            Ok(meta) if meta.is_dir() => {}
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(Error::InvalidPath(format!(
                    "{}: refused - a symbolic link is in the way",
                    here.display()
                )));
            }
            Ok(_) => {
                return Err(Error::InvalidPath(format!(
                    "{}: refused - a file is in the way",
                    here.display()
                )));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::create_dir(&here) {
                    Ok(()) => {}
                    // Somebody else got there first. Accept it only if what
                    // they made is a real directory.
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        let meta =
                            std::fs::symlink_metadata(&here).map_err(|e| Error::io(&here, e))?;
                        if !meta.is_dir() {
                            return Err(Error::InvalidPath(format!(
                                "{}: refused - something that is not a directory appeared here \
",
                                here.display()
                            )));
                        }
                    }
                    Err(e) => return Err(Error::io(&here, e)),
                }
            }
            Err(err) => return Err(Error::io(&here, err)),
        }
    }
    Ok(())
}

/// The directory a destination lives in, never above `dest_root`.
fn parent_of(dest_root: &Path, dest: &Path) -> PathBuf {
    match dest.parent() {
        Some(parent) if is_within(dest_root, parent) => parent.to_path_buf(),
        _ => dest_root.to_path_buf(),
    }
}

/// Remove whatever is at `dest` so a member can be written there, or refuse.
///
/// `remove_file` does not follow links, so a link that is in the way is
/// **replaced**, never written through. A directory is never removed on the
/// strength of a file's conflict answer (`ops::conflict`'s rule, the
/// design).
fn clear_dest(dest: &Path, overwrite: bool) -> Result<()> {
    match std::fs::symlink_metadata(dest) {
        Ok(meta) if meta.is_dir() => Err(Error::InvalidPath(format!(
            "{}: refused - a directory is already there",
            dest.display()
        ))),
        Ok(_) if overwrite => std::fs::remove_file(dest).map_err(|e| Error::io(dest, e)),
        Ok(_) => Err(Error::InvalidPath(format!(
            "{}: refused - it already exists",
            dest.display()
        ))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(Error::io(dest, err)),
    }
}

/// Create the file a member's bytes go into, never through a symbolic link.
///
/// `create_new` is `O_CREAT|O_EXCL`, which fails rather than following a link -
/// including a *dangling* one, which is the shape that defeats a plain
/// `File::create`. An existing destination is removed first, and only when the
/// caller's conflict policy said to.
pub fn create_file_within(dest_root: &Path, dest: &Path, overwrite: bool) -> Result<std::fs::File> {
    ensure_no_symlink_escape(dest_root, parent_of(dest_root, dest).as_path())?;
    clear_dest(dest, overwrite)?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .map_err(|e| Error::io(dest, e))
}

/// Create a symbolic link for a member, after checking where it points.
pub fn create_symlink_within(
    dest_root: &Path,
    dest: &Path,
    target: &str,
    overwrite: bool,
) -> Result<()> {
    ensure_no_symlink_escape(dest_root, parent_of(dest_root, dest).as_path())?;
    safe_link_target(&dest.to_string_lossy(), target)?;
    clear_dest(dest, overwrite)?;
    std::os::unix::fs::symlink(target, dest).map_err(|e| Error::io(dest, e))
}

/// Hard-link `dest` to `source`, both of which must be inside `dest_root`.
pub fn create_hardlink_within(
    dest_root: &Path,
    dest: &Path,
    source: &Path,
    overwrite: bool,
) -> Result<()> {
    ensure_no_symlink_escape(dest_root, parent_of(dest_root, dest).as_path())?;
    ensure_no_symlink_escape(dest_root, source)?;
    clear_dest(dest, overwrite)?;
    std::fs::hard_link(source, dest).map_err(|e| Error::io(dest, e))
}

/// Extract one planned member, whole: **the path every format extracts
/// through**.
///
/// Everything the design requires has already been decided by
/// [`Extraction::plan`]; this is where it is carried out, once, so that the zip
/// backend and the rar backend and the eighth format nobody has written yet
/// cannot each get it subtly differently.
///
/// * a streaming format's bytes pass through a [`GuardedWriter`], so a header
///   that lies about its size is caught while it lies;
/// * a materialising format (`.rar`, whose library will only extract to a path)
///   is checked against the same caps afterwards, and its output is removed if
///   it passed one;
/// * mode is applied through [`safe_mode`], and mtime is preserved.
///
///
/// Returns how many bytes landed.
pub fn extract(
    format: &dyn ArchiveFormat,
    container: &Path,
    member: &Member,
    planned: &Planned,
    extraction: &mut Extraction,
    overwrite: bool,
) -> Result<u64> {
    let root = extraction.root().to_path_buf();
    match &planned.action {
        Action::Dir { dest } => {
            create_dir_within(&root, dest)?;
            Ok(0)
        }
        Action::Symlink { dest, target } => {
            if let Some(parent) = dest.parent() {
                create_dir_within(&root, parent)?;
            }
            create_symlink_within(&root, dest, target, overwrite)?;
            Ok(0)
        }
        Action::Hardlink { dest, source } => {
            if let Some(parent) = dest.parent() {
                create_dir_within(&root, parent)?;
            }
            create_hardlink_within(&root, dest, source, overwrite)?;
            Ok(0)
        }
        Action::File {
            dest,
            declared,
            mode,
        } => {
            if let Some(parent) = dest.parent() {
                create_dir_within(&root, parent)?;
            }
            let (written, handle) = match format.member_source() {
                MemberSource::Stream => {
                    let file = create_file_within(&root, dest, overwrite)?;
                    let guard = extraction.guard(member.path.as_str(), Some(*declared));
                    let mut out = GuardedWriter::new(file, guard);
                    let outcome = format
                        .read_member(container, member, &mut out)
                        .and_then(|_| out.flush().map_err(Error::Bare));
                    let (file, guard) = out.into_inner();
                    if let Err(err) = outcome {
                        // Nothing half-written survives a refusal.
                        //
                        drop(file);
                        let _ = std::fs::remove_file(dest);
                        return Err(err);
                    }
                    // The handle is kept rather than the path reopened: mode
                    // and mtime then land on the file that was just written and
                    // not on whatever has appeared at that name since.
                    (guard.written(), Some(file))
                }
                MemberSource::Materialise => {
                    // The library writes the file itself, so the cap can only
                    // be applied on the other side of it.
                    ensure_no_symlink_escape(&root, parent_of(&root, dest).as_path())?;
                    clear_dest(dest, overwrite)?;
                    let claimed = format.extract_member(container, member, dest)?;
                    let actual = std::fs::symlink_metadata(dest)
                        .map(|meta| meta.len())
                        .unwrap_or(claimed);
                    let mut guard = extraction.guard(member.path.as_str(), Some(*declared));
                    if let Err(err) = guard.accept(actual) {
                        let _ = std::fs::remove_file(dest);
                        return Err(err);
                    }
                    (actual, None)
                }
            };
            match handle {
                Some(file) => {
                    if let Some(bits) = *mode {
                        use std::os::unix::fs::PermissionsExt as _;
                        file.set_permissions(std::fs::Permissions::from_mode(bits))
                            .map_err(|e| Error::io(dest, e))?;
                    }
                    // A timestamp the platform will not take is not worth
                    // failing an otherwise good extraction for.
                    if let Some(when) = member.mtime {
                        let _ = file.set_modified(when);
                    }
                }
                // A materialising format wrote the file itself and there is no
                // handle to it; the path is all there is.
                None => {
                    if let Some(bits) = *mode {
                        use std::os::unix::fs::PermissionsExt as _;
                        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(bits))
                            .map_err(|e| Error::io(dest, e))?;
                    }
                }
            }
            Ok(written)
        }
    }
}

/// Binary units, for a message a human reads.
///
/// A second copy of `session::human` rather than a shared one: that module is
/// owned elsewhere and its copy is private, and twelve lines of formatting is
/// not worth a cross-module dependency in the file whose whole point is that it
/// can be read in one sitting.
fn human(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes == u64::MAX {
        return "no limit".to_string();
    }
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit = unit.saturating_add(1);
    }
    let suffix = UNITS.get(unit).copied().unwrap_or("B");
    if unit == 0 {
        format!("{bytes} {suffix}")
    } else {
        format!("{value:.1} {suffix}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_and_parent_names_are_refused() {
        for name in [
            "/etc/passwd",
            "../../etc/passwd",
            "a/../../b",
            "a/..",
            "..",
            "C:\\Windows\\System32\\x",
            "\\\\server\\share\\x",
        ] {
            assert!(
                normalize_member(name, false).is_err(),
                "{name} must be refused"
            );
        }
        // Even a `..` that would have stayed inside is refused: the spec says
        // reject, not resolve.
        assert_eq!(
            normalize_member("a/b/../c", false),
            Err(Unsafe::ParentEscape)
        );
    }

    #[test]
    fn ordinary_names_normalise() {
        assert_eq!(normalize_member("a/b.txt", false).as_deref(), Ok("a/b.txt"));
        assert_eq!(normalize_member("./a//b/", false).as_deref(), Ok("a/b"));
        assert_eq!(normalize_member("a/b/", false).as_deref(), Ok("a/b"));
        // A backslash is an ordinary Unix filename character unless the format
        // says otherwise.
        assert_eq!(normalize_member("a\\b", false).as_deref(), Ok("a\\b"));
        assert_eq!(normalize_member("a\\b", true).as_deref(), Ok("a/b"));
        assert_eq!(normalize_member("", false), Err(Unsafe::Empty));
        assert_eq!(normalize_member("./", false), Err(Unsafe::Empty));
        assert_eq!(normalize_member("a\0b", false), Err(Unsafe::Nul));
        assert_eq!(
            normalize_member(&"x".repeat(MAX_MEMBER_COMPONENT + 1), false),
            Err(Unsafe::TooLong)
        );
        assert_eq!(
            normalize_member(&"a/".repeat(MAX_MEMBER_PATH), false),
            Err(Unsafe::TooLong)
        );
    }

    #[test]
    fn a_member_key_is_rootless_and_the_root_is_empty() {
        assert_eq!(member_key(Path::new("/")).ok().as_deref(), Some(""));
        assert_eq!(member_key(Path::new("")).ok().as_deref(), Some(""));
        assert_eq!(member_key(Path::new("/a/b")).ok().as_deref(), Some("a/b"));
        assert_eq!(member_key(Path::new("/a/b/")).ok().as_deref(), Some("a/b"));
        assert!(member_key(Path::new("/a/../b")).is_err());
    }

    #[test]
    fn a_destination_never_leaves_its_root() {
        let root = Path::new("/tmp/dest");
        assert_eq!(
            dest_path(root, "a/b.txt").ok(),
            Some(PathBuf::from("/tmp/dest/a/b.txt"))
        );
        for name in ["../x", "/etc/passwd", "a/../../x"] {
            assert!(dest_path(root, name).is_err(), "{name}");
        }
        assert!(is_within(root, Path::new("/tmp/dest/a")));
        assert!(!is_within(root, Path::new("/tmp/destination/a")));
        assert!(!is_within(root, Path::new("/tmp")));
    }

    #[test]
    fn a_link_target_that_leaves_is_refused() {
        assert!(safe_link_target("a/l", "b.txt").is_ok());
        assert!(safe_link_target("a/l", "sub/b.txt").is_ok());
        assert!(safe_link_target("a/l", "/etc/passwd").is_err());
        assert!(safe_link_target("a/l", "../../etc/passwd").is_err());
        assert!(safe_link_target("a/l", "a\0b").is_err());
    }

    #[test]
    fn a_symlink_in_the_destination_prefix_is_refused() {
        let root = std::env::temp_dir().join(format!("hcmd-slip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("real")).expect("create the destination");
        let outside = std::env::temp_dir().join(format!("hcmd-slip-out-{}", std::process::id()));
        std::fs::create_dir_all(&outside).expect("create the outside directory");
        std::os::unix::fs::symlink(&outside, root.join("evil")).expect("link");

        assert!(ensure_no_symlink_escape(&root, &root.join("real/x")).is_ok());
        assert!(ensure_no_symlink_escape(&root, &root.join("missing/x")).is_ok());
        assert!(
            ensure_no_symlink_escape(&root, &root.join("evil/x")).is_err(),
            "a link out of the destination must be refused"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    // -----------------------------------------------------------------------
    // The extraction path, exercised with archives crafted in the test.
    //
    // No fixture files: a hostile archive checked into the repository is a
    // hostile archive somebody eventually opens by accident, and one that is
    // built here says exactly what makes it hostile, in the test that cares.
    // -----------------------------------------------------------------------

    use crate::vfs::archive::index::{IndexSink, Locator, RawMember};
    use crate::vfs::archive::tar::TAR;
    use crate::vfs::archive::zip::ZipFormat;

    /// A private directory for one test.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hcmd-safety-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch directory");
        dir
    }

    /// A `.zip` with exactly these members, names stored verbatim.
    ///
    /// `zip` writes the name it is given, which is what makes a Zip Slip
    /// payload expressible here at all.
    fn craft_zip(path: &Path, members: &[(&str, &[u8])]) {
        let file = std::fs::File::create(path).expect("create");
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(0o644);
        for (name, body) in members {
            writer.start_file(*name, options).expect("start");
            writer.write_all(body).expect("write");
        }
        writer.finish().expect("finish");
    }

    /// One entry of a crafted tar.
    enum Craft<'a> {
        File(&'a str, &'a [u8]),
        Dir(&'a str),
        Symlink(&'a str, &'a str),
        Hardlink(&'a str, &'a str),
        CharDevice(&'a str),
    }

    /// Write `name` straight into the header's name field.
    ///
    /// `tar::Builder::append_data` refuses a `..` path - which is the library
    /// being careful, and exactly the reason the rule cannot be
    /// left to a library: a hostile tar is not written by `tar::Builder`. The
    /// name field is the first 100 bytes of the header.
    fn poke(header: &mut tar::Header, at: usize, text: &str) {
        let bytes = header.as_mut_bytes();
        for (i, byte) in text.bytes().enumerate() {
            if let Some(slot) = bytes.get_mut(at.saturating_add(i)) {
                *slot = byte;
            }
        }
    }

    /// A `.tar` holding exactly these entries, headers written by hand.
    fn craft_tar(path: &Path, items: &[Craft<'_>]) {
        let file = std::fs::File::create(path).expect("create");
        let mut builder = tar::Builder::new(file);
        for item in items {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o644);
            header.set_mtime(1_700_000_000);
            header.set_uid(0);
            header.set_gid(0);
            header.set_size(0);
            let body: &[u8] = match item {
                Craft::File(name, body) => {
                    header.set_entry_type(tar::EntryType::Regular);
                    header.set_size(body.len() as u64);
                    poke(&mut header, 0, name);
                    body
                }
                Craft::Dir(name) => {
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_mode(0o755);
                    poke(&mut header, 0, name);
                    &[]
                }
                Craft::Symlink(name, target) => {
                    header.set_entry_type(tar::EntryType::Symlink);
                    poke(&mut header, 0, name);
                    poke(&mut header, 157, target);
                    &[]
                }
                Craft::Hardlink(name, target) => {
                    header.set_entry_type(tar::EntryType::Link);
                    poke(&mut header, 0, name);
                    poke(&mut header, 157, target);
                    &[]
                }
                Craft::CharDevice(name) => {
                    header.set_entry_type(tar::EntryType::Char);
                    poke(&mut header, 0, name);
                    &[]
                }
            };
            header.set_cksum();
            builder.append(&header, body).expect("append");
        }
        builder.finish().expect("finish");
    }

    /// Every member a container holds, in its own order, **without** the
    /// index's normalisation - the hostile names included.
    #[derive(Default)]
    struct Collect(Vec<RawMember>);

    impl IndexSink for Collect {
        fn push(&mut self, raw: RawMember) -> bool {
            self.0.push(raw);
            true
        }

        fn cancelled(&self) -> bool {
            false
        }
    }

    fn raw_members(format: &dyn ArchiveFormat, container: &Path) -> Vec<Member> {
        let mut sink = Collect::default();
        format.index(container, &mut sink).expect("index");
        sink.0
            .into_iter()
            .map(|raw| Member {
                path: raw.name,
                kind: raw.kind,
                size: raw.size,
                mtime: raw.mtime,
                mode: raw.mode,
                uid: raw.uid,
                gid: raw.gid,
                locator: raw.locator,
                synthetic: false,
            })
            .collect()
    }

    fn size_of(path: &Path) -> u64 {
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    }

    /// Extract every member a container holds, reporting what was refused.
    fn extract_all(
        format: &dyn ArchiveFormat,
        container: &Path,
        dest: &Path,
        limits: ExtractLimits,
    ) -> Extraction {
        let mut extraction = Extraction::with_limits(dest, size_of(container), limits);
        for member in raw_members(format, container) {
            match extraction.plan(&member) {
                Ok(planned) => {
                    if let Err(err) =
                        extract(format, container, &member, &planned, &mut extraction, false)
                    {
                        extraction.refuse(&member.path, err.to_string());
                    }
                }
                Err(_) => {
                    // Already recorded, and the batch continues.
                }
            }
        }
        extraction
    }

    #[test]
    fn a_crafted_zip_slip_payload_never_reaches_the_filesystem() {
        let dir = scratch("zipslip");
        let container = dir.join("evil.zip");
        let dest = dir.join("dest");
        craft_zip(
            &container,
            &[
                ("../../../../tmp/hcmd-pwned", b"owned"),
                ("/etc/hcmd-pwned", b"owned"),
                ("a/../../b/hcmd-pwned", b"owned"),
                ("ok.txt", b"harmless"),
            ],
        );

        let extraction = extract_all(&ZipFormat, &container, &dest, ExtractLimits::default());

        assert_eq!(extraction.refused(), 3, "three escapes");
        assert_eq!(
            std::fs::read_to_string(dest.join("ok.txt")).ok().as_deref(),
            Some("harmless"),
            "the harmless member still lands: a refusal is per-file"
        );
        assert!(!Path::new("/tmp/hcmd-pwned").exists());
        assert!(!Path::new("/etc/hcmd-pwned").exists());
        assert!(!dir.join("b").exists(), "nothing beside the destination");

        // Every refusal names its entry and says why.
        let summary = extraction.summary().expect("a summary");
        for name in ["../../../../tmp/hcmd-pwned", "/etc/hcmd-pwned"] {
            assert!(summary.contains(name), "{summary}");
        }
        assert!(summary.contains("refused"), "{summary}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_index_refuses_the_same_names_before_extraction_ever_sees_them() {
        // Belt and braces, in that order: the index is the first refusal and
        // `Extraction` is the second, and neither is allowed to be the only one.
        let dir = scratch("twice");
        let container = dir.join("evil.zip");
        craft_zip(&container, &[("../../pwned", b"x"), ("ok.txt", b"y")]);
        let mut sink = crate::vfs::archive::index::Builder::new(
            std::sync::Arc::new(crate::vfs::archive::index::Index::new()),
            false,
        );
        assert!(ZipFormat.index(&container, &mut sink).is_ok());
        assert_eq!(
            raw_members(&ZipFormat, &container).len(),
            2,
            "both are in the container"
        );
        let mut extraction = Extraction::new(dir.join("dest"), size_of(&container));
        let hostile = Member {
            path: "../../pwned".to_string(),
            kind: MemberKind::File,
            size: 1,
            mtime: None,
            mode: 0o644,
            uid: 0,
            gid: 0,
            locator: Locator::Ordinal(0),
            synthetic: false,
        };
        let refusal = extraction.plan(&hostile).expect_err("refused");
        assert_eq!(refusal.member, "../../pwned");
        assert!(refusal.reason.contains("refused"), "{}", refusal.reason);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_symlink_out_of_the_destination_is_refused_and_so_is_writing_through_one() {
        let dir = scratch("linkescape");
        let container = dir.join("evil.tar");
        let dest = dir.join("dest");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&outside).expect("outside");

        // The two-entry attack: plant a link, then write through it.
        craft_tar(
            &container,
            &[
                Craft::Symlink("evil", "/etc"),
                Craft::File("evil/hcmd-pwned", b"owned"),
                Craft::Symlink("climb", "../../outside"),
                Craft::File("ok.txt", b"harmless"),
            ],
        );

        let extraction = extract_all(&TAR, &container, &dest, ExtractLimits::default());
        assert!(
            extraction.refused() >= 2,
            "the absolute link and the climbing one: {:?}",
            extraction.refusals()
        );
        // `evil` exists - as an ordinary directory, synthesised for the member
        // below it once the link that wanted that name was refused. What must
        // not exist is the link.
        let planted = std::fs::symlink_metadata(dest.join("evil")).expect("a directory");
        assert!(!planted.file_type().is_symlink(), "no link was planted");
        assert!(planted.is_dir());
        assert!(!Path::new("/etc/hcmd-pwned").exists());
        assert_eq!(
            std::fs::read_to_string(dest.join("evil/hcmd-pwned"))
                .ok()
                .as_deref(),
            Some("owned"),
            "the payload landed inside the destination, where it is harmless"
        );
        assert!(
            std::fs::read_dir(&outside)
                .expect("outside")
                .next()
                .is_none(),
            "and nothing reached the directory the links pointed at"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("ok.txt")).ok().as_deref(),
            Some("harmless")
        );

        // And the second half on its own: if a link somehow exists already -
        // another process, an earlier run, a format nobody has written yet -
        // the entry that would write through it is still refused. This is the
        // check a purely textual one cannot make.
        std::os::unix::fs::symlink(&outside, dest.join("via")).expect("plant");
        let member = Member {
            path: "via/hcmd-pwned".to_string(),
            kind: MemberKind::File,
            size: 5,
            mtime: None,
            mode: 0o644,
            uid: 0,
            gid: 0,
            locator: Locator::Offset { data: 0, len: 5 },
            synthetic: false,
        };
        let mut extraction = Extraction::new(&dest, size_of(&container));
        let refusal = extraction.plan(&member).expect_err("refused");
        assert!(
            refusal.reason.contains("symbolic link"),
            "{}",
            refusal.reason
        );
        assert!(!outside.join("hcmd-pwned").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_symlink_that_stays_inside_is_extracted() {
        let dir = scratch("linkok");
        let container = dir.join("ok.tar");
        let dest = dir.join("dest");
        craft_tar(
            &container,
            &[
                Craft::Dir("a"),
                Craft::File("a/real.txt", b"body"),
                Craft::Symlink("a/link.txt", "real.txt"),
            ],
        );
        let extraction = extract_all(&TAR, &container, &dest, ExtractLimits::default());
        assert_eq!(extraction.refused(), 0, "{:?}", extraction.refusals());
        assert!(dest.join("a").is_dir(), "the directory entry was created");
        let link = dest.join("a/link.txt");
        let meta = std::fs::symlink_metadata(&link).expect("the link exists");
        assert!(meta.file_type().is_symlink());
        assert_eq!(
            std::fs::read_to_string(&link).ok().as_deref(),
            Some("body"),
            "and it resolves inside the destination"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hardlink_is_refused_when_its_target_leaves_and_made_when_it_does_not() {
        let dir = scratch("hardlink");
        let container = dir.join("links.tar");
        let dest = dir.join("dest");
        craft_tar(
            &container,
            &[
                Craft::File("a.txt", b"body"),
                Craft::Hardlink("stolen", "../../../../etc/passwd"),
                Craft::Hardlink("rooted", "/etc/passwd"),
                Craft::Hardlink("fine", "a.txt"),
            ],
        );
        let extraction = extract_all(&TAR, &container, &dest, ExtractLimits::default());
        assert_eq!(extraction.refused(), 2, "{:?}", extraction.refusals());
        assert!(!dest.join("stolen").exists());
        assert!(!dest.join("rooted").exists());
        assert_eq!(
            std::fs::read_to_string(dest.join("fine")).ok().as_deref(),
            Some("body"),
            "a hard link inside the archive is a hard link"
        );
        let summary = extraction.summary().expect("a summary");
        assert!(summary.contains("stolen"), "{summary}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_declared_size_out_of_all_proportion_is_refused_before_a_byte_moves() {
        let dir = scratch("bomb");
        let container = dir.join("bomb.zip");
        let dest = dir.join("dest");
        // A megabyte of zeroes in a kilobyte of container: the shape of a
        // decompression bomb, at a size a test can afford.
        craft_zip(&container, &[("zeroes.bin", &vec![0u8; 1024 * 1024])]);
        assert!(size_of(&container) < 64 * 1024, "it really did compress");

        let limits = ExtractLimits {
            max_ratio: 10,
            ratio_floor: 1024,
            ..ExtractLimits::default()
        };
        let extraction = extract_all(&ZipFormat, &container, &dest, limits);
        assert_eq!(extraction.refused(), 1);
        let refusal = extraction.refusals().first().expect("one refusal");
        assert!(
            refusal.reason.contains("decompression bomb") && refusal.reason.contains(":1"),
            "the refusal says which cap and by how much: {}",
            refusal.reason
        );
        assert!(!dest.join("zeroes.bin").exists(), "nothing was written");

        // The same archive with the cap raised is an ordinary file.
        let dest = dir.join("dest2");
        let extraction = extract_all(&ZipFormat, &container, &dest, ExtractLimits::unbounded());
        assert_eq!(extraction.refused(), 0, "{:?}", extraction.refusals());
        assert_eq!(size_of(&dest.join("zeroes.bin")), 1024 * 1024);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn output_past_the_declared_size_is_refused_while_it_runs() {
        let dir = scratch("lying");
        let container = dir.join("liar.zip");
        let dest = dir.join("dest");
        craft_zip(&container, &[("big.bin", &vec![b'x'; 256 * 1024])]);

        // A zip's uncompressed-size field is attacker-controlled and the
        // deflate stream is not bound by it. This is that archive: a header
        // that says sixteen bytes in front of a quarter of a megabyte.
        let mut member = raw_members(&ZipFormat, &container)
            .into_iter()
            .next()
            .expect("one member");
        member.size = 16;

        let mut extraction = Extraction::new(&dest, size_of(&container));
        let planned = extraction.plan(&member).expect("the name is fine");
        let err = extract(
            &ZipFormat,
            &container,
            &member,
            &planned,
            &mut extraction,
            false,
        )
        .expect_err("the stream overruns its header");
        assert!(err.to_string().contains("lying size"), "{err}");
        assert!(
            !dest.join("big.bin").exists(),
            "no half-written destination"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_budget_is_charged_across_the_whole_extraction() {
        let dir = scratch("budget");
        let container = dir.join("many.zip");
        let dest = dir.join("dest");
        let body = vec![b'y'; 64 * 1024];
        craft_zip(
            &container,
            &[("one.bin", &body), ("two.bin", &body), ("three.bin", &body)],
        );

        let limits = ExtractLimits {
            max_total_bytes: 100 * 1024,
            ..ExtractLimits::unbounded()
        };
        let extraction = extract_all(&ZipFormat, &container, &dest, limits);
        assert!(extraction.refused() >= 1, "the budget stops the batch");
        let summary = extraction.summary().expect("a summary");
        assert!(summary.contains("budget"), "{summary}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_duplicate_name_is_a_conflict_and_not_a_silent_overwrite() {
        let dir = scratch("dup");
        let container = dir.join("dup.tar");
        let dest = dir.join("dest");
        // A tar carries the same name twice routinely - that is what an
        // incremental backup *is*. The index collapses them, but an extraction
        // walking the container must not quietly clobber. (`zip` 8.6 refuses to
        // *write* a duplicate name, which is why this one is a tar; a zip in
        // the wild can still hold them, and reaches the same code.)
        craft_tar(
            &container,
            &[
                Craft::File("dup.txt", b"first"),
                Craft::File("dup.txt", b"second"),
            ],
        );

        let members = raw_members(&TAR, &container);
        assert_eq!(members.len(), 2, "the container really holds two");
        let mut extraction = Extraction::new(&dest, size_of(&container));

        let first = extraction.plan(&members[0]).expect("planned");
        assert!(!first.duplicate);
        extract(
            &TAR,
            &container,
            &members[0],
            &first,
            &mut extraction,
            false,
        )
        .expect("the first lands");

        let second = extraction.plan(&members[1]).expect("planned");
        assert!(
            second.duplicate,
            "the second occurrence is flagged for the policy"
        );
        // Without an answer that says overwrite, it does not overwrite.
        assert!(
            extract(
                &TAR,
                &container,
                &members[1],
                &second,
                &mut extraction,
                false
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("dup.txt"))
                .ok()
                .as_deref(),
            Some("first")
        );
        // With one, it does.
        extract(
            &TAR,
            &container,
            &members[1],
            &second,
            &mut extraction,
            true,
        )
        .expect("overwrite");
        assert_eq!(
            std::fs::read_to_string(dest.join("dup.txt"))
                .ok()
                .as_deref(),
            Some("second")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_device_node_is_not_extracted_from_an_archive() {
        let dir = scratch("device");
        let container = dir.join("dev.tar");
        let dest = dir.join("dest");
        craft_tar(
            &container,
            &[Craft::CharDevice("dev/null"), Craft::File("ok.txt", b"y")],
        );
        let extraction = extract_all(&TAR, &container, &dest, ExtractLimits::default());
        assert_eq!(extraction.refused(), 1, "{:?}", extraction.refusals());
        let refusal = extraction.refusals().first().expect("one");
        assert!(refusal.reason.contains("device node"), "{}", refusal.reason);
        assert!(!dest.join("dev/null").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_and_a_dot_name_are_refused_by_the_extraction_too() {
        let dir = scratch("empty");
        let mut extraction = Extraction::new(dir.join("dest"), 1024);
        for name in ["", ".", "./", "a\0b", "C:\\x"] {
            let member = Member {
                path: name.to_string(),
                kind: MemberKind::File,
                size: 1,
                mtime: None,
                mode: 0,
                uid: 0,
                gid: 0,
                locator: Locator::Ordinal(0),
                synthetic: false,
            };
            let refusal = extraction.plan(&member).expect_err("refused");
            assert!(!refusal.reason.is_empty(), "{name} must say why");
        }
        assert_eq!(extraction.refused(), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn directories_are_created_without_walking_through_a_link() {
        let dir = scratch("mkdirp");
        let dest = dir.join("dest");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::create_dir_all(&dest).expect("dest");
        std::os::unix::fs::symlink(&outside, dest.join("via")).expect("plant");

        let err = create_dir_within(&dest, &dest.join("via/deep")).expect_err("refused");
        assert!(err.to_string().contains("symbolic link"), "{err}");
        assert!(!outside.join("deep").exists());

        create_dir_within(&dest, &dest.join("a/b/c")).expect("an ordinary tree");
        assert!(dest.join("a/b/c").is_dir());
        // Idempotent, which is what an archive listing `a/b/c.txt` and then
        // `a/b/` needs.
        create_dir_within(&dest, &dest.join("a/b/c")).expect("again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_replaces_a_link_rather_than_being_written_through_it() {
        let dir = scratch("nofollow");
        let dest = dir.join("dest");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::create_dir_all(&dest).expect("dest");
        let victim = outside.join("victim.txt");
        std::fs::write(&victim, "untouched").expect("victim");
        // A link *inside* the destination: `is_within` is satisfied and only
        // the open matters.
        std::os::unix::fs::symlink(&victim, dest.join("note.txt")).expect("plant");

        let mut file = create_file_within(&dest, &dest.join("note.txt"), true).expect("create");
        file.write_all(b"mine").expect("write");
        drop(file);
        assert_eq!(
            std::fs::read_to_string(&victim).ok().as_deref(),
            Some("untouched"),
            "the link was replaced, not followed"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("note.txt"))
                .ok()
                .as_deref(),
            Some("mine")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_extracted_file_is_never_setuid() {
        assert_eq!(safe_mode(0o4755), Some(0o755));
        assert_eq!(safe_mode(0o2755), Some(0o755));
        assert_eq!(safe_mode(0o1777), Some(0o777));
        assert_eq!(safe_mode(0), None, "a format with no mode says nothing");

        let dir = scratch("setuid");
        let container = dir.join("suid.tar");
        let dest = dir.join("dest");
        craft_tar(&container, &[Craft::File("shell", b"#!/bin/sh\n")]);
        let mut members = raw_members(&TAR, &container);
        let member = members.first_mut().expect("one");
        member.mode = 0o4755;
        let member = member.clone();

        let mut extraction = Extraction::new(&dest, size_of(&container));
        let planned = extraction.plan(&member).expect("planned");
        extract(&TAR, &container, &member, &planned, &mut extraction, false).expect("extract");
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(dest.join("shell"))
            .expect("stat")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o755, "setuid does not survive an unpack");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_summary_counts_every_refusal_and_shows_the_first_of_them() {
        let dir = scratch("summary");
        let mut extraction = Extraction::new(dir.join("dest"), 1024);
        assert!(
            extraction.summary().is_none(),
            "nothing refused, nothing said"
        );
        for i in 0..40 {
            extraction.refuse(format!("entry-{i}"), "refused - for the test");
        }
        let summary = extraction.summary().expect("a summary");
        assert!(
            summary.contains("40 entries were not extracted"),
            "{summary}"
        );
        assert!(summary.contains("entry-0"), "{summary}");
        assert!(summary.contains("and 32 more"), "{summary}");
        assert_eq!(extraction.refused(), 40);

        // The transcript is bounded; the count is not.
        for i in 0..200 {
            extraction.refuse(format!("more-{i}"), "refused - for the test");
        }
        assert_eq!(extraction.refused(), 240);
        assert_eq!(extraction.refusals().len(), MAX_RECORDED_REFUSALS);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_guard_reports_which_cap_it_was() {
        let extraction = Extraction::with_limits(
            "/nowhere",
            1024,
            ExtractLimits {
                max_member_bytes: 100,
                max_total_bytes: 150,
                ..ExtractLimits::unbounded()
            },
        );

        let mut guard = extraction.guard("a", Some(10));
        assert!(guard.accept(4).is_ok());
        let err = guard.accept(40).expect_err("past what it declared");
        assert!(err.to_string().contains("lying size"), "{err}");

        let mut guard = extraction.guard("b", None);
        let err = guard.accept(4096).expect_err("past the per-entry cap");
        assert!(err.to_string().contains("cap for one entry"), "{err}");

        let mut guard = extraction.guard("c", None);
        assert!(guard.accept(90).is_ok());
        let mut next = extraction.guard("d", None);
        let err = next.accept(90).expect_err("past the budget");
        assert!(err.to_string().contains("budget"), "{err}");
    }

    #[test]
    fn a_guarded_writer_stops_at_the_cap_rather_than_at_the_disk() {
        let extraction = Extraction::new("/nowhere", 1024);
        let guard = extraction.guard("m", Some(8));
        let mut out = GuardedWriter::new(Vec::new(), guard);
        assert!(out.write_all(b"12345678").is_ok());
        let err = out.write_all(b"9").expect_err("one byte too many");
        assert!(err.to_string().contains("lying size"), "{err}");
        let (bytes, guard) = out.into_inner();
        assert_eq!(bytes, b"12345678", "not one byte past the cap reached it");
        assert_eq!(guard.written(), 8);
    }
}
