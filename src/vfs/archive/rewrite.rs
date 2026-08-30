//! Writing into an archive without ever destroying it.
//!
//! > The rewrite itself writes to a temp file and renames over the original
//! > only on success, so an interrupted or failed rewrite never destroys the
//! > archive. The original is unlinked only after the rename succeeds.
//!
//! This module is the part of that promise that is the same for every format,
//! so no format has to reimplement it and none of them can disagree about it:
//!
//! * [`Rewrite`] is the temp-file-and-rename discipline. `rename(2)` over the
//!   original is *both* the install of the new container and the unlink of the
//!   old one, in one step the kernel cannot interrupt, which is why the order
//!   the design asks for is the only order this can happen in.
//! * [`TailGuard`] is the same promise for a **member-level** write, which by
//!   definition edits the container in place. A `.zip` append overwrites the
//!   central directory; a `.tar` append overwrites the end-of-archive blocks.
//!   Both are small and both are kept, so a failure - an I/O error, a cancelled
//!   job, a source file that vanished - puts them back.
//! * [`Plan`] turns a list of [`MemberEdit`]s into one question a format can
//!   ask about each member it meets: is this one dropped, or kept, and under
//!   what name. Every member path in it has been through
//!   [`super::safety::normalize_member`], so the rule holds for
//!   what is *written* as well as for what is read.
//! * [`verify_written`] re-opens what was just written and checks that what was
//!   asked for is in there. `Alt+F5`'s "move to archive" deletes the sources,
//!   and it may only do that after the pack has been read back
//!   rather than merely written.
//!
//! # What is not here
//!
//! the two gates - the size refusal and the free-space refusal -
//! live in [`super::session::RewriteLimits`], because they are answered *before*
//! anything is touched and one of the three answers is a dialog. This module
//! runs after that decision has been made.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use crate::error::{Error, Result};

use super::format::{ArchiveFormat, MemberEdit, WriteProgress};
use super::index::{IndexSink, RawMember};
use super::safety::normalize_member;

/// How many bytes one copy step moves before it reports progress and asks
/// whether it may continue.
///
/// The same 64 KiB [`super::stream::copy_exact`] uses. Small enough that `Esc`
/// is answered promptly (the design wants cancellation honoured "within a
/// large file's chunk loop"), large enough that the syscall overhead is noise.
pub const COPY_CHUNK: usize = 64 * 1024;

/// The most bytes [`TailGuard`] will hold in memory before a caller has to fall
/// back to a full repack.
///
/// A zip's central directory is about 50 bytes per entry, so 32 MiB is roughly
/// 600 000 members - more than [`super::index::MAX_MEMBERS`] would ever list
/// anyway. Past it, holding the tail is a memory bound nobody agreed to, and
/// the honest answer is to write a new container instead.
pub const MAX_TAIL_SALVAGE: u64 = 32 * 1024 * 1024;

/// Nonce for temp names, so two rewrites of two archives in one directory
/// cannot collide.
static NONCE: AtomicU64 = AtomicU64::new(0);

/// Binary units, for a message a human reads.
fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
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

/// Free bytes on the filesystem holding `path`.
///
/// "Available space comes from `sysinfo`, matching the temp path
/// against the longest mount point; no new dependency." The same call
/// [`super::session`] makes, for the same reason.
fn free_space(path: &Path) -> Option<u64> {
    crate::ui::volume::for_path(path).map(|volume| volume.free)
}

/// Refuse a rewrite the filesystem holding `beside` cannot hold, reporting the
/// numbers.
///
/// [`super::session::RewriteLimits::gate`] asks this question of
/// `archive.temp_dir`, which is where the design points it. A rewrite has to
/// land on the filesystem holding the *archive*, though - that is what makes
/// the final `rename` atomic - so the same question is asked again here, of the
/// filesystem that will actually have to hold both copies. Two different
/// filesystems are the case where asking once is asking the wrong one.
pub fn ensure_room(beside: &Path, needed: u64) -> Result<()> {
    let dir = beside.parent().unwrap_or(Path::new("."));
    if let Some(free) = free_space(dir)
        && free < needed
    {
        return Err(Error::msg(format!(
            "rewriting {} needs {}, {} free on {}",
            beside.display(),
            human(needed),
            human(free),
            dir.display(),
        )));
    }
    Ok(())
}

/// What the design requires free for a rewrite of `size`: the new archive
/// beside the original, plus a tenth.
pub fn rewrite_footprint(size: u64) -> u64 {
    size.saturating_mul(2)
        .saturating_add(size.saturating_div(5))
}

/// What marks a file beside an archive as a rewrite in progress.
///
/// The name is `.{archive}{TEMP_INFIX}{pid}-{nonce}`, and the pid in it is
/// what lets [`sweep_dead_rewrites`] tell this session's work from a dead
/// session's litter.
const TEMP_INFIX: &str = ".hcmd-rewrite-";

/// Is a process with this id still running?
pub(crate) fn pid_is_live(pid: u32) -> bool {
    pid == std::process::id() || Path::new(&format!("/proc/{pid}")).exists()
}

/// Remove rewrite temp files for this archive left by processes that are gone.
///
/// A `SIGKILL` part way through a rewrite runs no destructor, so the temp file
/// survives - hidden, beside the user's archive, and as large as the rewrite
/// had got. The archive itself is safe (that is the whole point of writing
/// beside it), but nothing would ever remove the remains: the design step
/// 10's startup sweep covers the session cache, and this file is not in it.
///
/// So the sweep happens here, where a rewrite of this very archive is about to
/// start: the directory is one that is being written to anyway, only names
/// belonging to this archive are considered, and a temp file whose owner is
/// still running is left strictly alone - a second hcmd rewriting the same
/// archive must not have its work deleted underneath it.
fn sweep_dead_rewrites(dir: &Path, prefix: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix(prefix) else {
            continue;
        };
        // `{pid}-{nonce}`. Anything else is not one of ours.
        let Some((pid, nonce)) = rest.split_once('-') else {
            continue;
        };
        let (Ok(pid), true) = (pid.parse::<u32>(), !nonce.is_empty()) else {
            continue;
        };
        if pid_is_live(pid) {
            continue;
        }
        let _ = std::fs::remove_file(entry.path());
    }
}

/// A new container being written beside an existing one.
///
/// The temp file is created **in the target's own directory**, not in
/// `archive.temp_dir`: `rename(2)` only works within one filesystem, and a
/// cross-device move is a copy, which is exactly the non-atomic step the design
/// exists to avoid. Dropping a `Rewrite` that was not committed removes the
/// temp file and leaves the original untouched, so a cancelled job, a failed
/// read and a panic all end the same way.
#[derive(Debug)]
pub struct Rewrite {
    target: PathBuf,
    temp: PathBuf,
    committed: bool,
}

impl Rewrite {
    /// Prepare to replace `target`, which need not exist yet (`Alt+F5` creates
    /// a new archive through the same discipline, so a pack that fails part way
    /// leaves no half-written file behind either).
    pub fn beside(target: &Path) -> Result<Self> {
        let dir = target.parent().unwrap_or(Path::new("."));
        let name = target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "archive".to_string());
        let prefix = format!(".{name}{TEMP_INFIX}");
        let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
        let temp = dir.join(format!("{prefix}{}-{nonce:x}", std::process::id()));
        // A leftover from a killed process is not ours to keep.
        let _ = std::fs::remove_file(&temp);
        sweep_dead_rewrites(dir, &prefix);
        Ok(Self {
            target: target.to_path_buf(),
            temp,
            committed: false,
        })
    }

    /// Where the new container is being written.
    pub fn path(&self) -> &Path {
        &self.temp
    }

    /// The archive this will become.
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// Create the temp file, refusing to reuse one that is already there.
    ///
    /// The original's permission bits are copied onto it **before** a byte is
    /// written, so a `0600` archive is never briefly a `0644` one. Ownership
    /// cannot be preserved without privileges and is not attempted; the new
    /// container belongs to whoever ran the rewrite, which is the same rule
    /// `mv` follows.
    pub fn create(&self) -> Result<File> {
        use std::os::unix::fs::OpenOptionsExt as _;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            // Private until the mode is known: an archive can hold anything.
            .mode(0o600)
            .open(&self.temp)
            .map_err(|e| Error::io(&self.temp, e))?;
        if let Ok(meta) = std::fs::metadata(&self.target) {
            use std::os::unix::fs::MetadataExt as _;
            let _ = std::fs::set_permissions(
                &self.temp,
                <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(
                    meta.mode() & 0o7777,
                ),
            );
        } else {
            // A brand-new archive gets the ordinary default rather than the
            // private mode the file was created with.
            let _ = std::fs::set_permissions(
                &self.temp,
                <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o644),
            );
        }
        Ok(file)
    }

    /// Install what was written over the original.
    ///
    /// The bytes are on the platter before the rename, and the rename is what
    /// unlinks the original - there is no window in which neither file is a
    /// whole archive.
    pub fn commit(mut self) -> Result<()> {
        {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&self.temp)
                .map_err(|e| Error::io(&self.temp, e))?;
            file.sync_all().map_err(|e| Error::io(&self.temp, e))?;
        }
        std::fs::rename(&self.temp, &self.target).map_err(|e| Error::io(&self.target, e))?;
        self.committed = true;
        // And the directory entry itself, so the replacement survives a power
        // cut rather than only a crash. Best effort: some filesystems refuse a
        // directory fsync, and a rewrite that has already landed must not fail
        // because of it.
        if let Some(dir) = self.target.parent() {
            let _ = File::open(dir).and_then(|d| d.sync_all());
        }
        Ok(())
    }
}

impl Drop for Rewrite {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.temp);
        }
    }
}

/// The bytes a member-level write is about to overwrite, kept so that a failure
/// can put them back.
///
/// the design wants `.zip` and uncompressed `.tar` edited in place rather
/// than rewritten; the design wants a failed write never to destroy the
/// archive. Both hold at once because the only region an append overwrites is
/// the container's *trailing metadata* - a zip's central directory, a tar's
/// end-of-archive blocks - which is small, bounded by [`MAX_TAIL_SALVAGE`], and
/// therefore cheap to keep until the write has succeeded.
#[derive(Debug)]
pub struct TailGuard {
    path: PathBuf,
    from: u64,
    bytes: Vec<u8>,
    original_len: u64,
    armed: bool,
}

impl TailGuard {
    /// Keep everything in `path` from `from` to the end.
    ///
    /// Returns `Ok(None)` when that is more than [`MAX_TAIL_SALVAGE`]; the
    /// caller must then write a whole new container instead of editing this one
    /// in place, because holding the tail would be an unbounded allocation and
    /// letting it go would be an unprotected write.
    pub fn capture(path: &Path, from: u64) -> Result<Option<Self>> {
        Self::capture_bounded(path, from, MAX_TAIL_SALVAGE)
    }

    /// [`TailGuard::capture`] with the bound given explicitly.
    ///
    /// A parameter only so a test can reach the fallback without building a
    /// 32 MiB central directory, the same reason
    /// [`super::index::Builder::with_limits`] exists; every caller outside a
    /// test gets [`MAX_TAIL_SALVAGE`].
    pub fn capture_bounded(path: &Path, from: u64, most: u64) -> Result<Option<Self>> {
        let mut file = File::open(path).map_err(|e| Error::io(path, e))?;
        let original_len = file
            .seek(SeekFrom::End(0))
            .map_err(|e| Error::io(path, e))?;
        if from > original_len {
            return Err(Error::msg(format!(
                "{}: the archive is {original_len} bytes but its entries end at {from}",
                path.display()
            )));
        }
        if original_len.saturating_sub(from) > most {
            return Ok(None);
        }
        file.seek(SeekFrom::Start(from))
            .map_err(|e| Error::io(path, e))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|e| Error::io(path, e))?;
        Ok(Some(Self {
            path: path.to_path_buf(),
            from,
            bytes,
            original_len,
            armed: true,
        }))
    }

    /// The write succeeded: there is nothing to put back.
    pub fn disarm(&mut self) {
        self.armed = false;
    }

    /// Put the archive back exactly as it was.
    pub fn restore(&self) -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&self.path)
            .map_err(|e| Error::io(&self.path, e))?;
        file.seek(SeekFrom::Start(self.from))
            .map_err(|e| Error::io(&self.path, e))?;
        file.write_all(&self.bytes)
            .map_err(|e| Error::io(&self.path, e))?;
        file.set_len(self.original_len)
            .map_err(|e| Error::io(&self.path, e))?;
        file.sync_all().map_err(|e| Error::io(&self.path, e))?;
        Ok(())
    }
}

impl Drop for TailGuard {
    /// A guard that was never disarmed is a write that never finished - a
    /// panic, an early `?`, a cancelled job - and the archive goes back.
    ///
    /// Every step of [`TailGuard::restore`] can fail - the seek, the write,
    /// the truncate, the `fsync` - and a full disk or a read-only remount
    /// fails all four. When that happens the archive is left with its central
    /// directory or its end-of-archive blocks overwritten, which is the one
    /// outcome this type exists to prevent, and a `let _ =` here meant nobody
    /// was ever told. A `Drop` cannot return, so the outcome goes where a
    /// caller can find it: [`take_unrestored_tails`].
    fn drop(&mut self) {
        if self.armed
            && let Err(err) = self.restore()
        {
            record_unrestored(&self.path, &err);
        }
    }
}

/// One archive a [`TailGuard`] could not put back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnrestoredTail {
    /// The archive, left with its trailing metadata overwritten.
    pub path: PathBuf,
    /// Why the restore failed, already phrased for a failure summary.
    pub error: String,
}

/// The archives whose tails could not be put back, oldest first.
///
/// A module-level sink rather than a field, because the failure happens in a
/// `Drop` that has nowhere to return to and no caller to hand a value back
/// through. A rewrite that ends this way has damaged the container, so the
/// job that ran it drains this and reports every entry as a failure of its
/// own - see [`take_unrestored_tails`].
static UNRESTORED: std::sync::Mutex<Vec<UnrestoredTail>> = std::sync::Mutex::new(Vec::new());

/// The most entries [`UNRESTORED`] holds.
///
/// A bound rather than none: nothing drains this on a run that never opens an
/// archive again, and a message nobody has read yet must not be able to grow
/// without limit. Damage past the cap is dropped rather than the damage
/// already recorded, because the first report is the one that explains what
/// happened.
const MAX_UNRESTORED: usize = 64;

/// Note that `path` was left damaged, for [`take_unrestored_tails`] to hand on.
///
/// Silent about its own failure - a poisoned lock here would mean a thread
/// panicked while recording damage, and there is no third place to report
/// that to.
fn record_unrestored(path: &Path, error: &Error) {
    if let Ok(mut damaged) = UNRESTORED.lock()
        && damaged.len() < MAX_UNRESTORED
    {
        damaged.push(UnrestoredTail {
            path: path.to_path_buf(),
            error: error.to_string(),
        });
    }
}

/// Take every archive a [`TailGuard`] could not put back, emptying the record.
///
/// Draining rather than reading, so one damaged archive is reported once. A
/// caller that has finished a rewrite calls this and fails the job for
/// anything it finds: the write is already undone as far as it is going to
/// be, and the only thing left to do about it is say so.
#[must_use]
pub fn take_unrestored_tails() -> Vec<UnrestoredTail> {
    UNRESTORED
        .lock()
        .map(|mut damaged| std::mem::take(&mut *damaged))
        .unwrap_or_default()
}

/// One addition a [`Plan`] makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Put {
    /// The normalised member path.
    pub member_path: String,
    /// The local file holding the bytes; `None` for a directory member.
    pub source: Option<PathBuf>,
    /// Mode bits to record. `0` means "the format's default", which is
    /// [`Put::file_mode`].
    pub mode: u32,
    /// Modification time to record.
    pub mtime: Option<SystemTime>,
}

impl Put {
    /// Is this a directory member?
    pub fn is_dir(&self) -> bool {
        self.source.is_none()
    }

    /// The mode to actually write, with the format's default filled in.
    pub fn file_mode(&self) -> u32 {
        match (self.mode & 0o7777, self.is_dir()) {
            (0, true) => 0o755,
            (0, false) => 0o644,
            (mode, _) => mode,
        }
    }

    /// How many bytes the source holds right now, `0` for a directory.
    ///
    /// A *claim*, in the same sense a header's size is: the file can change
    /// under us, which is why every writer here streams exactly this many bytes
    /// through [`ExactReader`] and fails rather than writing a container whose
    /// headers disagree with its data.
    pub fn size(&self) -> Result<u64> {
        match &self.source {
            None => Ok(0),
            Some(path) => std::fs::metadata(path)
                .map(|m| m.len())
                .map_err(|e| Error::io(path, e)),
        }
    }
}

/// What happens to a member already in the container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fate {
    /// It is not in the new container: removed, or replaced by an addition.
    Drop,
    /// It is kept, under this name - the same one unless it was renamed.
    Keep(String),
}

/// A set of [`MemberEdit`]s, resolved into one question per existing member.
///
/// Built once per write. Every path in it has been normalised and validated by
/// [`normalize_member`], so a format cannot write a member whose name would
/// escape on extraction - the rule applied to the write side, where
/// it is just as real: an archive hcmd produced must not be a Zip Slip payload
/// for whoever unpacks it next.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    removed: Vec<String>,
    renamed: Vec<(String, String)>,
    puts: Vec<Put>,
}

impl Plan {
    /// Resolve `edits`, refusing any member name that could escape.
    pub fn new(edits: &[MemberEdit]) -> Result<Self> {
        let mut plan = Self::default();
        for edit in edits {
            match edit {
                MemberEdit::Put {
                    member_path,
                    source,
                    mode,
                    mtime,
                } => plan.puts.push(Put {
                    member_path: check(member_path)?,
                    source: Some(source.clone()),
                    mode: *mode,
                    mtime: *mtime,
                }),
                MemberEdit::PutDir { member_path, mode } => plan.puts.push(Put {
                    member_path: check(member_path)?,
                    source: None,
                    mode: *mode,
                    mtime: None,
                }),
                MemberEdit::Remove { member_path } => plan.removed.push(check(member_path)?),
                MemberEdit::Rename { from, to } => {
                    let (from, to) = (check(from)?, check(to)?);
                    if from != to {
                        plan.renamed.push((from, to));
                    }
                }
            }
        }
        Ok(plan)
    }

    /// Is there nothing to do?
    pub fn is_empty(&self) -> bool {
        self.removed.is_empty() && self.renamed.is_empty() && self.puts.is_empty()
    }

    /// Does this plan only *add*, leaving every existing member where it is?
    ///
    /// The precondition for a member-level append. It is not sufficient on its
    /// own: an addition whose name is already in the container replaces that
    /// member, which no append can express, so a caller also checks
    /// [`Plan::puts`] against the names it found.
    pub fn adds_only(&self) -> bool {
        self.removed.is_empty() && self.renamed.is_empty()
    }

    /// The additions, in the order they were asked for.
    pub fn puts(&self) -> &[Put] {
        &self.puts
    }

    /// Every member path this plan expects to find afterwards, for
    /// [`verify_written`].
    pub fn added_paths(&self) -> Vec<String> {
        self.puts.iter().map(|p| p.member_path.clone()).collect()
    }

    /// What becomes of the existing member at `member_path`.
    ///
    /// A removal or a rename of a directory takes everything beneath it -
    /// the "directories are recursed with a single confirmation",
    /// applied inside an archive where there is no second entry to recurse
    /// into.
    pub fn fate(&self, member_path: &str) -> Fate {
        for removed in &self.removed {
            if covers(removed, member_path) {
                return Fate::Drop;
            }
        }
        let mut name = member_path.to_string();
        for (from, to) in &self.renamed {
            if member_path == from {
                name = to.clone();
                break;
            }
            if let Some(rest) = member_path.strip_prefix(&format!("{from}/")) {
                name = format!("{to}/{rest}");
                break;
            }
        }
        // An addition replaces whatever was at its name, so the old member does
        // not survive into the new container.
        if self.puts.iter().any(|p| p.member_path == name) {
            return Fate::Drop;
        }
        Fate::Keep(name)
    }
}

/// Does `prefix` name `path`, or a directory containing it?
fn covers(prefix: &str, path: &str) -> bool {
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

/// Validate one member path from an edit.
fn check(raw: &str) -> Result<String> {
    normalize_member(raw, false).map_err(|why| {
        Error::InvalidPath(format!(
            "{raw}: refused - {} cannot be written into an archive",
            why.reason()
        ))
    })
}

/// Tell `progress` that `n` more bytes have landed, turning a refusal into
/// [`Error::Cancelled`].
///
/// cancellation is honoured "within a large file's chunk loop".
/// A format that gets this error abandons the rewrite; because the rewrite is
/// in a temp file (or behind a [`TailGuard`]), abandoning it is all it has to
/// do.
pub fn note(progress: &mut dyn WriteProgress, n: u64) -> Result<()> {
    if progress.bytes(n) {
        Ok(())
    } else {
        Err(Error::Cancelled)
    }
}

/// Copy `reader` to `out` until it ends, reporting progress and stopping on
/// cancellation.
pub fn copy_watched(
    reader: &mut dyn Read,
    out: &mut dyn Write,
    progress: &mut dyn WriteProgress,
) -> Result<u64> {
    let mut buf = [0u8; COPY_CHUNK];
    let mut total = 0u64;
    loop {
        let read = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Error::Bare(e)),
        };
        let chunk = buf.get(..read).unwrap_or(&[]);
        out.write_all(chunk).map_err(Error::Bare)?;
        total = total.saturating_add(read as u64);
        note(progress, read as u64)?;
    }
    Ok(total)
}

/// A reader that yields exactly `len` bytes or fails.
///
/// `tar::Builder::append` writes the header and then whatever the reader
/// produces, without checking the two agree - a source file that shrank between
/// its `stat` and its read would silently produce a tar whose headers lie.
/// Wrapping the source makes that a reported error instead. A source that grew
/// is truncated to the size the header claims, because the header is what the
/// container has already promised.
#[derive(Debug)]
pub struct ExactReader<R> {
    inner: R,
    left: u64,
    what: String,
}

impl<R: Read> ExactReader<R> {
    /// Wrap `inner`, expecting exactly `len` bytes from it.
    pub fn new(inner: R, len: u64, what: impl Into<String>) -> Self {
        Self {
            inner,
            left: len,
            what: what.into(),
        }
    }
}

impl<R: Read> Read for ExactReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            return Ok(0);
        }
        let want = usize::try_from(self.left.min(buf.len() as u64)).unwrap_or(buf.len());
        let slice = buf.get_mut(..want).unwrap_or(&mut []);
        let read = self.inner.read(slice)?;
        if read == 0 {
            return Err(std::io::Error::other(format!(
                "{}: ended {} bytes before the {} it was declared to hold",
                self.what,
                self.left,
                if self.left == 1 { "byte" } else { "bytes" }
            )));
        }
        self.left = self.left.saturating_sub(read as u64);
        Ok(read)
    }
}

/// A reader that reports what it produced and stops when a job is cancelled.
///
/// Used where the *library* drives the copy - `tar::Builder::append` and
/// `zip`'s raw copy both take a reader and do their own `io::copy` - so this is
/// the only place the bytes can be counted (the rule that a rewrite
/// is the one case the copy loop above the `Vfs` trait never sees).
pub struct WatchedReader<'a, R> {
    inner: R,
    progress: &'a mut dyn WriteProgress,
}

impl<R> std::fmt::Debug for WatchedReader<'_, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WatchedReader")
    }
}

impl<'a, R: Read> WatchedReader<'a, R> {
    /// Report everything `inner` produces to `progress`.
    pub fn new(inner: R, progress: &'a mut dyn WriteProgress) -> Self {
        Self { inner, progress }
    }
}

impl<R: Read> Read for WatchedReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let cap = buf.len().min(COPY_CHUNK);
        let slice = buf.get_mut(..cap).unwrap_or(&mut []);
        let read = self.inner.read(slice)?;
        if read > 0 && !self.progress.bytes(read as u64) {
            return Err(std::io::Error::other(CANCELLED));
        }
        Ok(read)
    }
}

/// The marker a [`WatchedReader`] puts in the `io::Error` it cannot otherwise
/// type, so the format can turn it back into [`Error::Cancelled`].
const CANCELLED: &str = "the archive write was cancelled";

/// Turn an error a library produced back into [`Error::Cancelled`] when it was
/// really this crate's own cancellation travelling through it.
///
/// `tar` and `zip` both wrap whatever the reader returned in their own error
/// type, so a cancelled job would otherwise reach the user as "I/O error"
/// rather than as the cancellation it is - and the design makes that
/// distinction load-bearing: a cancelled operation is not a failed one.
pub fn unwrap_cancellation(err: Error) -> Error {
    if err.to_string().contains(CANCELLED) {
        Error::Cancelled
    } else {
        err
    }
}

/// Every member path in `path`, as the index would normalise them.
///
/// A name that could not be normalised is *not* returned: it never exists as
/// far as the rest of the crate is concerned, so a write must
/// not treat it as a name that is already taken either.
pub fn member_names(format: &dyn ArchiveFormat, path: &Path) -> Result<Vec<String>> {
    let mut sink = Collector::default();
    format.index(path, &mut sink)?;
    Ok(sink.names)
}

/// Re-open what was just written and check that what was asked for is in it.
///
/// Two things are worth this second pass, and they are the two places a write
/// can cost somebody data:
///
/// * the design lets `Alt+F5` "pack then delete sources". Deleting the only
///   copy of something on the strength of a write having returned `Ok` is one
///   buffered error away from data loss.
/// * A full rewrite renames its result **over** a working
///   archive. "An interrupted or failed rewrite never destroys the archive"
///   only holds if a rewrite that produced an unreadable container counts as
///   failed - which it can only do if something reads it.
///
/// So the archive is read back through exactly the code that will read it
/// tomorrow, the format's own `index`, and only then is it renamed into place
/// or are the sources deleted. `at_least` is how many members the caller knows
/// it wrote, which catches a container that parses but stops early.
pub fn verify_written(
    format: &dyn ArchiveFormat,
    path: &Path,
    expected: &[String],
    at_least: usize,
) -> Result<()> {
    let found = member_names(format, path)?;
    for want in expected {
        if !found.iter().any(|got| got == want) {
            return Err(Error::msg(format!(
                "{}: the archive was written but {want} cannot be read back out of it \
; nothing was replaced or deleted",
                path.display()
            )));
        }
    }
    if found.len() < at_least {
        return Err(Error::msg(format!(
            "{}: {at_least} members were written but only {} can be read back \
; nothing was replaced or deleted",
            path.display(),
            found.len(),
        )));
    }
    Ok(())
}

/// An [`IndexSink`] that only collects names, for [`member_names`].
#[derive(Debug, Default)]
struct Collector {
    names: Vec<String>,
}

impl IndexSink for Collector {
    fn push(&mut self, raw: RawMember) -> bool {
        if let Ok(name) = normalize_member(&raw.name, false) {
            self.names.push(name);
        }
        true
    }

    fn cancelled(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::archive::format::NoProgress;

    fn temp(tag: &str) -> PathBuf {
        let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "hcmd-rewrite-{tag}-{}-{nonce:x}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn a_rewrite_that_is_not_committed_leaves_the_original_alone() {
        let dir = temp("abandon");
        let target = dir.join("a.tar.gz");
        std::fs::write(&target, b"the original archive").expect("write");

        {
            let rewrite = Rewrite::beside(&target).expect("beside");
            let mut file = rewrite.create().expect("create");
            file.write_all(b"half a new archive").expect("write");
            assert!(rewrite.path().exists());
            // Dropped without `commit`, exactly as a cancelled or failed
            // rewrite does.
        }

        assert_eq!(
            std::fs::read(&target).expect("read"),
            b"the original archive",
            "an interrupted rewrite never destroys the archive"
        );
        assert_eq!(
            std::fs::read_dir(&dir).expect("read_dir").count(),
            1,
            "and leaves no temp file behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_committed_rewrite_replaces_the_original_and_keeps_its_mode() {
        let dir = temp("commit");
        let target = dir.join("a.zip");
        std::fs::write(&target, b"old").expect("write");
        std::fs::set_permissions(
            &target,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
        )
        .expect("chmod");

        let rewrite = Rewrite::beside(&target).expect("beside");
        let mut file = rewrite.create().expect("create");
        file.write_all(b"new").expect("write");
        drop(file);
        rewrite.commit().expect("commit");

        assert_eq!(std::fs::read(&target).expect("read"), b"new");
        let mode = <std::fs::Metadata as std::os::unix::fs::MetadataExt>::mode(
            &std::fs::metadata(&target).expect("stat"),
        );
        assert_eq!(mode & 0o777, 0o600, "a private archive stays private");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tail_guard_puts_the_container_back() {
        let dir = temp("tail");
        let path = dir.join("a.zip");
        std::fs::write(&path, b"DATA....CENTRAL-DIRECTORY").expect("write");
        let original = std::fs::read(&path).expect("read");

        {
            let mut guard = TailGuard::capture(&path, 8)
                .expect("capture")
                .expect("small");
            // An append overwrites the tail and then fails.
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .expect("open");
            file.seek(SeekFrom::Start(8)).expect("seek");
            file.write_all(b"XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX")
                .expect("write");
            drop(file);
            assert_ne!(std::fs::read(&path).expect("read"), original);
            guard.restore().expect("restore");
            guard.disarm();
        }
        assert_eq!(std::fs::read(&path).expect("read"), original);

        // And the drop path does it without being asked.
        {
            let _guard = TailGuard::capture(&path, 8)
                .expect("capture")
                .expect("small");
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .expect("open");
            file.seek(SeekFrom::Start(8)).expect("seek");
            file.write_all(b"YYYY").expect("write");
        }
        assert_eq!(
            std::fs::read(&path).expect("read"),
            original,
            "an undisarmed guard restores on drop"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The two tests below share `UNRESTORED`, which is process-wide, and both
    /// of them *drain* it. Run in parallel they take each other's evidence:
    /// one records a damaged archive, the other's drain removes it, and the
    /// first then fails saying a failed restore left no trace. The record is
    /// global because damage is global, so the tests take turns instead.
    fn unrestored_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn a_restore_that_fails_on_drop_is_recorded_rather_than_swallowed() {
        let _serialised = unrestored_lock();
        // The archive whose tail cannot be put back is the one case this type
        // exists to prevent, and `let _ = self.restore()` meant it happened in
        // total silence: the central directory stays overwritten and the job
        // reports success.
        //
        // A container that has gone away stands in for the full disk and the
        // read-only remount - `restore` opens for writing without creating,
        // so the open fails the way `ENOSPC` fails the write, and what is
        // under test is what the drop does with the error either way.
        let dir = temp("tail-damage");
        let path = dir.join("a.zip");
        std::fs::write(&path, b"DATA....CENTRAL-DIRECTORY").expect("write");

        // Whatever else the suite left behind is not this test's evidence.
        let _ = take_unrestored_tails();
        {
            let _guard = TailGuard::capture(&path, 8)
                .expect("capture")
                .expect("small");
            std::fs::remove_file(&path).expect("remove");
        }

        let damaged = take_unrestored_tails();
        let mine = damaged.iter().find(|entry| entry.path == path);
        assert!(
            mine.is_some(),
            "a failed restore left no trace: {damaged:?}"
        );
        assert!(
            mine.is_some_and(|entry| !entry.error.is_empty()),
            "the record has to say why: {damaged:?}"
        );
        // And draining it means one damaged archive is reported once.
        assert!(
            !take_unrestored_tails().iter().any(|e| e.path == path),
            "the record was not drained"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_restore_that_succeeds_on_drop_records_nothing() {
        let _serialised = unrestored_lock();
        // The other half: the ordinary undisarmed drop puts the tail back, and
        // a warning about an archive that is intact would send someone
        // looking for damage that is not there.
        let dir = temp("tail-quiet");
        let path = dir.join("a.zip");
        std::fs::write(&path, b"DATA....CENTRAL-DIRECTORY").expect("write");

        let _ = take_unrestored_tails();
        {
            let _guard = TailGuard::capture(&path, 8)
                .expect("capture")
                .expect("small");
        }

        assert!(
            !take_unrestored_tails().iter().any(|e| e.path == path),
            "a restore that worked was reported as damage"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tail_too_big_to_hold_declines_rather_than_allocating() {
        let dir = temp("bigtail");
        let path = dir.join("a.bin");
        std::fs::write(&path, b"x".repeat(64)).expect("write");

        // Within the bound: the tail is held, so the write it protects can go
        // ahead in place.
        assert!(
            TailGuard::capture_bounded(&path, 0, 64)
                .expect("capture")
                .is_some_and(|guard| guard.bytes.len() == 64)
        );
        // Past it: no guard, so the caller writes a whole new container rather
        // than editing this one without a way back.
        assert!(
            TailGuard::capture_bounded(&path, 0, 63)
                .expect("capture")
                .is_none()
        );
        // And a `from` past the end of the file is a container that disagrees
        // with itself, which is an error and not a zero-length tail.
        assert!(TailGuard::capture_bounded(&path, 65, 1024).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_plan_refuses_a_name_that_could_escape() {
        for name in ["../evil", "/etc/passwd", "a/../b", ""] {
            let edits = [MemberEdit::Put {
                member_path: name.to_string(),
                source: PathBuf::from("/dev/null"),
                mode: 0o644,
                mtime: None,
            }];
            assert!(
                Plan::new(&edits).is_err(),
                "{name} must be refused on the write side too"
            );
        }
    }

    #[test]
    fn a_removal_takes_everything_beneath_it() {
        let plan = Plan::new(&[MemberEdit::Remove {
            member_path: "d".to_string(),
        }])
        .expect("plan");
        assert_eq!(plan.fate("d"), Fate::Drop);
        assert_eq!(plan.fate("d/a.txt"), Fate::Drop);
        assert_eq!(plan.fate("d/e/f.txt"), Fate::Drop);
        assert_eq!(plan.fate("dd/a.txt"), Fate::Keep("dd/a.txt".to_string()));
        assert!(!plan.adds_only());
    }

    #[test]
    fn a_rename_takes_everything_beneath_it_and_an_addition_replaces() {
        let plan = Plan::new(&[
            MemberEdit::Rename {
                from: "old".to_string(),
                to: "new".to_string(),
            },
            MemberEdit::Put {
                member_path: "keep/me.txt".to_string(),
                source: PathBuf::from("/dev/null"),
                mode: 0,
                mtime: None,
            },
        ])
        .expect("plan");
        assert_eq!(plan.fate("old"), Fate::Keep("new".to_string()));
        assert_eq!(plan.fate("old/a.txt"), Fate::Keep("new/a.txt".to_string()));
        assert_eq!(
            plan.fate("keep/me.txt"),
            Fate::Drop,
            "the addition replaces it"
        );
        assert_eq!(plan.puts().len(), 1);
        assert_eq!(plan.puts()[0].file_mode(), 0o644, "the format's default");
        assert!(!plan.adds_only());
    }

    #[test]
    fn an_exact_reader_refuses_a_source_that_shrank() {
        let mut reader = ExactReader::new(std::io::Cursor::new(b"four".to_vec()), 4096, "a.txt");
        let mut out = Vec::new();
        let err = std::io::copy(&mut reader, &mut out).expect_err("short");
        assert!(err.to_string().contains("ended"), "{err}");
        assert_eq!(out, b"four", "what there was is still written");

        let mut reader = ExactReader::new(std::io::Cursor::new(b"twelve bytes".to_vec()), 6, "b");
        let mut out = Vec::new();
        std::io::copy(&mut reader, &mut out).expect("exact");
        assert_eq!(out, b"twelve", "a source that grew is cut to the header");
    }

    #[test]
    fn cancelling_is_reported_as_cancellation_and_not_as_an_io_error() {
        struct Stop;
        impl WriteProgress for Stop {
            fn bytes(&mut self, _n: u64) -> bool {
                false
            }
        }
        let mut out = Vec::new();
        let err = copy_watched(
            &mut std::io::Cursor::new(vec![0u8; 4096]),
            &mut out,
            &mut Stop,
        )
        .expect_err("cancelled");
        assert!(matches!(err, Error::Cancelled), "{err}");

        let mut stop = Stop;
        let mut reader = WatchedReader::new(std::io::Cursor::new(vec![0u8; 4096]), &mut stop);
        let mut sink = Vec::new();
        let io_err = std::io::copy(&mut reader, &mut sink).expect_err("cancelled");
        assert!(matches!(
            unwrap_cancellation(Error::Bare(io_err)),
            Error::Cancelled
        ));
    }

    #[test]
    fn progress_counts_what_was_written() {
        struct Count(u64);
        impl WriteProgress for Count {
            fn bytes(&mut self, n: u64) -> bool {
                self.0 = self.0.saturating_add(n);
                true
            }
        }
        let mut counter = Count(0);
        let mut out = Vec::new();
        let moved = copy_watched(
            &mut std::io::Cursor::new(vec![7u8; 200_000]),
            &mut out,
            &mut counter,
        )
        .expect("copy");
        assert_eq!(moved, 200_000);
        assert_eq!(counter.0, 200_000);
        assert!(NoProgress.bytes(1), "and the null reporter never cancels");
    }

    #[test]
    fn the_footprint_is_double_plus_a_tenth() {
        // "require `archive_size × 2` plus a 10% margin".
        assert_eq!(rewrite_footprint(1000), 2200);
        assert_eq!(rewrite_footprint(0), 0);
        // And it saturates rather than wrapping on a hostile size.
        assert!(rewrite_footprint(u64::MAX) >= u64::MAX / 2);
    }

    #[test]
    fn human_sizes_read_as_the_spec_writes_them() {
        assert_eq!(human(300 * 1024 * 1024), "300.0 MiB");
        assert_eq!(human(12), "12 B");
    }

    // -----------------------------------------------------------------------
    // The write half through the `Vfs` trait, which is what the design is
    // actually claiming: "archives are directories", so `F5` into one, `F8`
    // inside one and `F2` on a member are the code that already exists.
    // -----------------------------------------------------------------------

    use crate::vfs::archive::{ArchiveFs, ArchiveSession, RewriteLimits};
    use crate::vfs::{BackendKind, Vfs, VfsPath};
    use std::sync::Arc;

    fn zip_with(path: &Path, members: &[(&str, &[u8])]) {
        let file = File::create(path).expect("create");
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(0o644);
        for (name, body) in members {
            writer.start_file(*name, options).expect("start");
            writer.write_all(body).expect("write");
        }
        writer.finish().expect("finish");
    }

    fn tar_gz_with(path: &Path, members: &[(&str, &[u8])]) {
        let file = File::create(path).expect("create");
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        for (name, body) in members {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(1_700_000_000);
            header.set_cksum();
            builder
                .append_data(&mut header, name, std::io::Cursor::new(*body))
                .expect("append");
        }
        builder.into_inner().expect("tar").finish().expect("gz");
    }

    fn inside(container: &Path, member: &str) -> VfsPath {
        VfsPath::local(container).with_segment(BackendKind::Archive, member)
    }

    fn open(session: &Arc<ArchiveSession>, container: &Path) -> Arc<ArchiveFs> {
        let fs = session.open(&inside(container, "/")).expect("open");
        fs.wait_for_index();
        fs
    }

    fn contents(fs: &ArchiveFs, container: &Path, member: &str) -> Option<Vec<u8>> {
        let mut reader = fs.open_read(&inside(container, member)).ok()?;
        let mut out = Vec::new();
        reader.read_to_end(&mut out).ok()?;
        Some(out)
    }

    #[test]
    fn f5_into_an_archive_commits_on_flush_and_a_dropped_write_changes_nothing() {
        // Contract invariant 12: `flush` is the commit, so the "no
        // half-written destination" holds for a member as it does for a file.
        let dir = temp("vfs-write");
        let container = dir.join("a.zip");
        zip_with(&container, &[("was.txt", b"already here")]);
        let session =
            ArchiveSession::in_dir(dir.join("cache"), RewriteLimits::default()).expect("session");
        let fs = open(&session, &container);

        // Dropped without a flush: nothing changed.
        let before = std::fs::read(&container).expect("read");
        {
            let mut writer = fs
                .open_write(&inside(&container, "/never.txt"))
                .expect("open_write");
            writer.write_all(b"abandoned").expect("write");
        }
        assert_eq!(std::fs::read(&container).expect("read"), before);

        // Flushed: committed.
        let mut writer = fs
            .open_write(&inside(&container, "/added/new.txt"))
            .expect("open_write");
        writer.write_all(b"the new member").expect("write");
        writer.flush().expect("flush commits");
        // A second flush is a no-op rather than a second commit.
        writer.flush().expect("idempotent");
        drop(writer);

        fs.wait_for_index();
        assert_eq!(
            contents(&fs, &container, "/added/new.txt").as_deref(),
            Some(b"the new member".as_slice())
        );
        assert_eq!(
            contents(&fs, &container, "/was.txt").as_deref(),
            Some(b"already here".as_slice())
        );
        assert!(
            fs.index().get("never.txt").is_none(),
            "the abandoned write never happened"
        );
        drop(fs);
        drop(session);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn f8_inside_an_archive_deletes_from_it_and_f2_renames_within_it() {
        // "`F8` inside an archive deletes from it, same
        // capability rules." Both arrive here as `Vfs::remove` and
        // `Vfs::rename` on the archive backend, unchanged from what a local
        // directory gets.
        let dir = temp("vfs-delete");
        let container = dir.join("a.zip");
        zip_with(
            &container,
            &[
                ("keep.txt", b"keep"),
                ("drop/one.txt", b"1"),
                ("drop/two.txt", b"2"),
                ("old.txt", b"renamed"),
            ],
        );
        let session =
            ArchiveSession::in_dir(dir.join("cache"), RewriteLimits::default()).expect("session");
        let fs = open(&session, &container);

        fs.remove(&inside(&container, "/drop")).expect("remove");
        fs.wait_for_index();
        assert!(
            fs.index().get("drop/one.txt").is_none(),
            "a directory takes what is in it"
        );
        assert!(fs.index().get("drop/two.txt").is_none());

        fs.rename(
            &inside(&container, "/old.txt"),
            &inside(&container, "/new.txt"),
        )
        .expect("rename");
        fs.wait_for_index();
        assert_eq!(
            contents(&fs, &container, "/new.txt").as_deref(),
            Some(b"renamed".as_slice())
        );
        assert!(fs.index().get("old.txt").is_none());
        assert_eq!(
            contents(&fs, &container, "/keep.txt").as_deref(),
            Some(b"keep".as_slice())
        );
        drop(fs);
        drop(session);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_rewrite_gate_refuses_before_a_byte_is_touched() {
        // the first gate, reached through the backend rather than
        // through the dialog: a caller that skipped the dialog is refused, and
        // the message says why and what to do instead.
        let dir = temp("vfs-gate");
        let container = dir.join("a.tar.gz");
        tar_gz_with(&container, &[("was.txt", b"already here")]);
        let before = std::fs::read(&container).expect("read");
        let session = ArchiveSession::in_dir(dir.join("cache"), RewriteLimits { warn: 1, max: 8 })
            .expect("session");
        let fs = open(&session, &container);

        let mut writer = fs
            .open_write(&inside(&container, "/new.txt"))
            .expect("open_write");
        writer.write_all(b"never lands").expect("write");
        let refusal = writer.flush().expect_err("refused");
        let message = refusal.to_string();
        assert!(message.contains("rewriting"), "{message}");
        assert!(
            message.contains("Extract it, change it, and repack it deliberately"),
            "the design wants the suggestion, not just the refusal: {message}"
        );
        assert_eq!(
            std::fs::read(&container).expect("read"),
            before,
            "a refusal touches nothing"
        );
        drop(writer);
        drop(fs);
        drop(session);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_compressed_tar_is_rewritten_through_the_trait_and_the_index_is_rebuilt() {
        // Contract invariant 15: every `Locator` addressed a position in a
        // container that no longer exists, so the index is thrown away and
        // rebuilt rather than patched.
        let dir = temp("vfs-rewrite");
        let container = dir.join("a.tar.gz");
        tar_gz_with(&container, &[("first.txt", b"one"), ("second.txt", b"two")]);
        let session =
            ArchiveSession::in_dir(dir.join("cache"), RewriteLimits::default()).expect("session");
        let fs = open(&session, &container);

        let mut writer = fs
            .open_write(&inside(&container, "/third.txt"))
            .expect("open_write");
        writer.write_all(b"three").expect("write");
        writer.flush().expect("flush commits");
        drop(writer);

        fs.wait_for_index();
        for (member, want) in [
            ("/first.txt", b"one".as_slice()),
            ("/second.txt", b"two".as_slice()),
            ("/third.txt", b"three".as_slice()),
        ] {
            assert_eq!(
                contents(&fs, &container, member).as_deref(),
                Some(want),
                "{member} reads through the rebuilt index"
            );
        }
        drop(fs);
        drop(session);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A rewrite temp file left by a process that is gone gets swept; one
    /// belonging to a process that is still running does not.
    ///
    /// `SIGKILL` runs no destructor, so this is the only thing that ever
    /// removes the remains of an interrupted rewrite. It has to be exact: the
    /// file it must not touch is another hcmd's rewrite of the same archive,
    /// in progress right now.
    #[test]
    fn a_dead_rewrite_is_swept_and_a_live_one_is_not() {
        let dir = temp("sweep");
        let target = dir.join("a.tar.gz");
        std::fs::write(&target, b"the original archive").expect("write");

        // Pid 0 is never a live process, so this one is litter.
        let dead = dir.join(".a.tar.gz.hcmd-rewrite-0-1");
        std::fs::write(&dead, b"half a rewrite from a killed session").expect("write");
        // This process is very much alive, so this one is somebody's work.
        let live = dir.join(format!(
            ".a.tar.gz.hcmd-rewrite-{}-ffff",
            std::process::id()
        ));
        std::fs::write(&live, b"a rewrite in progress").expect("write");
        // A different archive's temp file, and a file that merely looks close.
        let other = dir.join(".b.tar.gz.hcmd-rewrite-0-1");
        std::fs::write(&other, b"another archive's rewrite").expect("write");
        let innocent = dir.join(".a.tar.gz.hcmd-rewrite-notapid");
        std::fs::write(&innocent, b"not one of ours at all").expect("write");

        let rewrite = Rewrite::beside(&target).expect("beside");

        assert!(!dead.exists(), "a dead session's temp file was left behind");
        assert!(live.exists(), "a live session's rewrite was deleted");
        assert!(other.exists(), "another archive's temp file was deleted");
        assert!(innocent.exists(), "an unrelated file was deleted");
        assert_eq!(
            std::fs::read(&target).ok().as_deref(),
            Some(&b"the original archive"[..]),
            "the sweep touched the archive"
        );
        drop(rewrite);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
