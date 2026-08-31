//! ISO 9660, with Joliet and Rock Ridge, on `hadris-iso`.
//!
//! One reader, no state. [`Iso9660`] is a zero-sized value behind
//! [`VolumeFormat`], exactly as `archive::format::ArchiveFormat`'s
//! implementations are: everything that lives longer than a call belongs to
//! `ImageFs` and to the session, and the container is opened, read and closed
//! inside each method. There is no write half to disable, because
//! [`VolumeFormat`] has no write method to implement.
//!
//! # What an ISO can do that a naive walk gets wrong
//!
//! Every one of these is a thing the bytes of an image can say, and the bytes
//! of an image are chosen by whoever wrote it:
//!
//! 1. `.` and `..` are the records whose identifier is the single byte `0x00`
//!    or `0x01`, and they are skipped by identity before a name is decoded.
//!    A Rock Ridge `NM` entry can *also* spell `..` in a record that is not
//!    one of those two, so the decoded name is checked again with
//!    [`crate::vfs::is_plain_name`] (the design I2, I8).
//! 2. A name is the Rock Ridge `NM` alternate name where there is one, else
//!    the Joliet or ISO identifier with its `;1` version suffix and any
//!    trailing NUL removed. It then goes through `index::Builder`, which is
//!    where an absolute name, a `..`, an over-long component and the refusal
//!    count all live, once, unforked.
//! 3. Mode, owner and group come from the Rock Ridge `PX` entry through
//!    [`crate::vfs::untrusted_mode`] and are `0` without one. An ISO without
//!    Rock Ridge has no POSIX metadata at all and says so by leaving it zero
//!    rather than by inventing `0o644`.
//! 4. A Rock Ridge `SL` target becomes [`MemberKind::Symlink`], carrying the
//!    target exactly as the image stored it; it is validated at read time by
//!    `safety::safe_link_target`, never followed here. A relocated directory
//!    placeholder (`RE`) is skipped by the library and a child link (`CL`) is
//!    followed once, against a visited set of directory extents.
//! 5. An extent is refused unless [`Region::contains`] agrees. A directory
//!    record is 253 bytes of numbers from the image and an extent past the end
//!    of it is the ordinary result of a truncated download
//!    (the design I4).
//! 6. A member whose record declares interleaving is refused rather than read
//!    contiguously, because its bytes are not contiguous and reading them as
//!    if they were produces a file that is wrong without being obviously
//!    wrong.
//!
//! # Why the walk is a queue rather than a recursion
//!
//! A directory in a crafted image can be its own ancestor: nothing in ISO 9660
//! stops a directory record from naming an extent that has already been
//! walked, and `hadris-iso` does not check. The walk is therefore an explicit
//! stack bounded by [`MAX_DIR_DEPTH`], against a visited set of extents, so a
//! cycle costs one directory rather than the process
//! (the design I8).

use std::collections::HashSet;
use std::io::Write;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hadris_iso::directory::DirectoryRef;
use hadris_iso::file::EntryType;
use hadris_iso::read::{DirEntry, IsoImage, RootDir};
use hadris_iso::{ErrorKind as IsoErrorKind, Read as IsoRead, Seek as IsoSeek};

use crate::error::{Error, Result};
use crate::vfs::archive::index::{IndexSink, Locator, Member, MemberKind, RawMember};
use crate::vfs::{Capabilities, LatencyClass, ReadSeek, is_plain_name, untrusted_mode};

use super::block::{Reader, Region, copy_range};
use super::format::{FsId, MAX_DIR_DEPTH, VolumeFormat};

/// The ISO 9660 reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Iso9660;

/// How many records one listing steps over before it gives up on the image.
///
/// A record `hadris-iso` refuses to hand over is skipped and the walk carries
/// on, which is only safe while the library really has stepped past it. This
/// is the bound that makes "carry on" terminate whether it did or not.
const MAX_SKIPPED_RECORDS: usize = 1024;

/// The logical block an ISO 9660 extent is counted in.
///
/// `hadris-iso` refuses an image whose primary volume descriptor declares
/// anything else, by name, so this is a constant rather than a field and the
/// refusal is passed through rather than worked around.
pub const LOGICAL_BLOCK: u64 = 2048;

impl VolumeFormat for Iso9660 {
    fn id(&self) -> FsId {
        FsId::Iso9660
    }

    fn capabilities(&self) -> Capabilities {
        // `seekable` and `random_access` are true because a single-extent
        // member is a contiguous byte range of a local file and reading part
        // of it is one `pread`. `writable` is false because that is the
        // feature and not a stage of it.
        Capabilities {
            writable: false,
            seekable: true,
            random_access: true,
            has_directories: true,
            atomic_rename: false,
            paged_listing: false,
            can_execute: false,
            links: false,
            settable_mode: false,
            latency: LatencyClass::Local,
        }
    }

    fn index(&self, region: &Region, sink: &mut dyn IndexSink) -> Result<()> {
        let image = open_image(region)?;
        let root = root_of(&image)?;
        let names = NameSource::of(root.entry_type());

        // The root's own extent is visited before the walk starts, so an image
        // whose subdirectory points back at the root stops there.
        let mut visited: HashSet<usize> = HashSet::new();
        let root_ref = root.dir_ref();
        let _ = visited.insert(root_ref.extent.0);
        let mut queue: Vec<(DirectoryRef, String, usize)> = vec![(root_ref, String::new(), 0)];
        // The first thing that went wrong, reported once at the end rather
        // than at the point it happened, so that every directory the image
        // *can* produce is still listed. `spawn_index` turns this into
        // `IndexStatus::Failed`, which keeps what was read on the panel and
        // says the rest is missing (an unreadable thing is never
        // shown as an empty one).
        let mut failure: Option<String> = None;
        let mut skipped = 0usize;

        while let Some((dir_ref, prefix, depth)) = queue.pop() {
            if sink.cancelled() {
                return Ok(());
            }
            // Checked before anything is queued as well; this is the root's
            // own turn at it, and a second answer costs one comparison.
            if !dir_within(region, &dir_ref) {
                failure.get_or_insert_with(|| {
                    "the root directory is recorded outside the volume".to_string()
                });
                continue;
            }
            let dir = image.open_dir(dir_ref);
            for item in dir.entries() {
                if sink.cancelled() {
                    return Ok(());
                }
                let entry = match item {
                    Ok(entry) => entry,
                    // One record this library will not hand over, which today
                    // means an interleaved one (rule 6). The reader has
                    // already stepped past it, so the rest of the directory
                    // still lists and only that member is missing. Bounded,
                    // because "the library says no and I carry on" is a loop
                    // if it ever stops stepping past.
                    Err(why)
                        if why.kind() == IsoErrorKind::Unsupported
                            && skipped < MAX_SKIPPED_RECORDS =>
                    {
                        skipped = skipped.saturating_add(1);
                        failure.get_or_insert_with(|| {
                            let at = if prefix.is_empty() { "/" } else { &prefix };
                            format!("{at}: a file in this directory cannot be read ({why})")
                        });
                        continue;
                    }
                    // Anything else leaves the rest of this directory at an
                    // unknown offset, so the directory ends here and the queue
                    // carries on with the ones that are still readable.
                    Err(why) => {
                        failure.get_or_insert_with(|| {
                            let at = if prefix.is_empty() { "/" } else { &prefix };
                            format!("{at}: this directory is damaged ({why})")
                        });
                        break;
                    }
                };
                if entry.is_special() {
                    continue;
                }
                let name = member_name(&entry, names);
                if !is_plain_name(&name) {
                    continue;
                }
                if !push_entry(sink, &prefix, &name, &entry, region) {
                    return Ok(());
                }
                if !entry.is_directory() || depth.saturating_add(1) >= MAX_DIR_DEPTH {
                    continue;
                }
                let here = join(&prefix, &name);
                // A directory that is listed and never walked would show as an
                // empty one, which is the thing the design forbids. Both ways
                // that can happen say so instead, and the walk carries on with
                // the directories that are still readable.
                let child = match entry.as_dir_ref(&image) {
                    Ok(child) => child,
                    Err(why) => {
                        failure.get_or_insert_with(|| {
                            format!("{here}: this directory cannot be read ({why})")
                        });
                        continue;
                    }
                };
                if !dir_within(region, &child) {
                    failure.get_or_insert_with(|| {
                        format!("{here}: this directory is recorded outside the volume it is in")
                    });
                    continue;
                }
                if !visited.insert(child.extent.0) {
                    continue;
                }
                queue.push((child, here, depth.saturating_add(1)));
            }
        }
        match failure {
            Some(why) => Err(Error::msg(why)),
            None => Ok(()),
        }
    }

    fn read_member(&self, region: &Region, member: &Member, out: &mut dyn Write) -> Result<u64> {
        match member.locator {
            Locator::Offset { data, len } => copy_range(region, data, len, out),
            // A member with more than one extent, which means one larger than
            // 4 GiB. Its bytes are not one range, so there is nothing to
            // remember and it is found again by walking to it.
            Locator::None => read_by_walking(region, member, out),
            Locator::Ordinal(_) => Err(Error::Unsupported("reading this member of a disk image")),
        }
    }

    fn open_member(
        &self,
        region: &Region,
        member: &Member,
    ) -> Result<Option<Box<dyn ReadSeek + Send>>> {
        match member.locator {
            Locator::Offset { data, len } if region.contains(data, len) => {
                let window = region.slice(data, len)?;
                Ok(Some(Box::new(window.open()?)))
            }
            Locator::Offset { .. } | Locator::None | Locator::Ordinal(_) => Ok(None),
        }
    }

    fn volume_label(&self, region: &Region) -> Option<String> {
        let image = open_image(region).ok()?;
        let pvd = image.read_pvd().ok()?;
        let label = pvd.volume_identifier.try_to_str().ok()?.trim();
        if label.is_empty() {
            None
        } else {
            Some(label.to_string())
        }
    }
}

/// Which namespace a directory tree's identifiers are written in.
///
/// The Rock Ridge `NM` name wins over both of these where there is one; this
/// is what to do with the identifier in the record itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NameSource {
    /// An ISO 9660 identifier: uppercase, `;1`-suffixed, and a bare `.` where
    /// a name has no extension.
    Iso,
    /// A Joliet identifier: UTF-16 big endian.
    Joliet,
}

impl NameSource {
    /// Which one a directory tree of this type carries.
    fn of(ty: EntryType) -> Self {
        match ty {
            EntryType::Joliet { .. } => Self::Joliet,
            EntryType::Level1 { .. } | EntryType::Level2 { .. } | EntryType::Level3 { .. } => {
                Self::Iso
            }
        }
    }
}

/// Open the ISO on `region`, read-only.
///
/// The handle borrows nothing outside the call and is dropped with it: a
/// `Vfs` is `Send + Sync` and `IsoImage` guards its cursor with a
/// `spin::Mutex`, which is bounded here only because every lock it takes is
/// held across one block read of a local file and never across an await.
///
fn open_image(region: &Region) -> Result<IsoImage<Terminator>> {
    let reader = Terminator::over(region)?;
    IsoImage::open(reader).map_err(|e| Error::msg(format!("the ISO 9660 volume is damaged: {e}")))
}

/// The first sector of the volume descriptor set, which ECMA-119 fixes.
const FIRST_DESCRIPTOR: u64 = 16;

/// How far the descriptor scan looks for the terminator before giving up.
///
/// ECMA-119 puts no ceiling on the descriptor set, so this is one: a chain
/// that has not terminated in 64 sectors is not going to, and the scan is not
/// the place to read an image's worth of sectors looking.
const MAX_DESCRIPTORS: u64 = 64;

/// A [`Reader`] that presents the volume descriptor set terminator's body as
/// ECMA-119 says it already is.
///
/// **A deviation, recorded rather than hidden.** ECMA-119 §8.3 reserves every
/// byte of the terminator after its 7-byte header "for future
/// standardisation" and requires them to be zero. `hadris-iso` 2.2.0 enforces
/// that literally (`volume.rs`: "volume descriptor set terminator body is not
/// zero-filled") and refuses the whole image when it does not hold. Real
/// mastering tools leave bytes there anyway: a retail Windows 2000 SP4 disc
/// carries four of them in the last word of the sector, and the kernel's
/// `isofs` reads it without complaint because it never looks. Refusing such a
/// disc would be reporting a damaged image for a field whose content the
/// standard says has no meaning, which is the mistake the design draws a line
/// under: an unreadable image and an unsupported one are different things to
/// say, and this is neither.
///
/// So exactly one sector's reserved body reads as zero, and nothing else is
/// touched. This is not leniency about the image's *content*: no directory
/// record, no extent and no name passes through the mask, because the mask is
/// a byte range fixed before the first read and it is the terminator's, whose
/// bytes carry nothing to be lenient about.
#[derive(Debug)]
pub struct Terminator {
    inner: Reader,
    /// The half-open byte range presented as zero: the terminator's body.
    /// `None` when the scan found no terminator, in which case this is a
    /// plain [`Reader`] and `hadris-iso` fails the image on its own terms.
    mask: Option<(u64, u64)>,
    /// Where `inner` stands, tracked so a read knows which of its bytes fall
    /// in the mask without asking for the position on every call.
    pos: u64,
}

impl Terminator {
    /// Open `region` and find the terminator, if it has one.
    ///
    /// One pass over at most [`MAX_DESCRIPTORS`] sectors, each read once and
    /// none of them kept. A descriptor whose identifier is not `CD001` ends
    /// the scan without a mask: that is not a volume descriptor set, and
    /// `hadris-iso` is the thing that should say so.
    pub(super) fn over(region: &Region) -> Result<Self> {
        let mask = Self::find(region).unwrap_or(None);
        Ok(Self {
            inner: region.open()?,
            mask,
            pos: 0,
        })
    }

    /// The terminator's body, as a byte range of the region.
    pub(super) fn find(region: &Region) -> Result<Option<(u64, u64)>> {
        let mut reader = region.open()?;
        let mut sector = [0u8; LOGICAL_BLOCK as usize];
        for index in 0..MAX_DESCRIPTORS {
            let at = FIRST_DESCRIPTOR
                .saturating_add(index)
                .saturating_mul(LOGICAL_BLOCK);
            if !region.contains(at, LOGICAL_BLOCK) {
                return Ok(None);
            }
            // Fully qualified, both of them: `hadris-iso` blanket-implements
            // its own `Read` and `Seek` for every `std::io` type, so a bare
            // `reader.seek(..)` is ambiguous and the inference that resolves
            // it picks the crate's error type rather than this module's.
            if std::io::Seek::seek(&mut reader, std::io::SeekFrom::Start(at)).is_err() {
                return Ok(None);
            }
            if std::io::Read::read_exact(&mut reader, &mut sector).is_err() {
                return Ok(None);
            }
            let Some(header) = sector.get(..7) else {
                return Ok(None);
            };
            if header.get(1..6) != Some(b"CD001") {
                return Ok(None);
            }
            // Type 255 is the set terminator. Its body is everything after
            // the header, and that body is what this masks.
            if header.first() == Some(&255) {
                return Ok(Some((
                    at.saturating_add(7),
                    at.saturating_add(LOGICAL_BLOCK),
                )));
            }
        }
        Ok(None)
    }
}

impl std::io::Read for Terminator {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let start = self.pos;
        let read = std::io::Read::read(&mut self.inner, buf)?;
        self.pos = self.pos.saturating_add(read as u64);
        let Some((from, to)) = self.mask else {
            return Ok(read);
        };
        // The overlap of what was just read with the masked range, in the
        // buffer's own coordinates. Saturating throughout: an empty overlap
        // is the ordinary case and must cost nothing and touch nothing.
        let low = start.max(from);
        let high = start.saturating_add(read as u64).min(to);
        if low < high {
            let at = usize::try_from(low.saturating_sub(start)).unwrap_or(0);
            let len = usize::try_from(high.saturating_sub(low)).unwrap_or(0);
            if let Some(slice) = buf.get_mut(at..at.saturating_add(len)) {
                slice.fill(0);
            }
        }
        Ok(read)
    }
}

impl std::io::Seek for Terminator {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.pos = std::io::Seek::seek(&mut self.inner, pos)?;
        Ok(self.pos)
    }
}

/// Which directory tree to walk.
///
/// The primary tree when the image carries Rock Ridge, because that is where
/// the `NM` names, the `PX` modes and the `SL` symlink targets are; the
/// crate's own best choice otherwise, which is Joliet when there is one. This
/// is the choice the kernel's `isofs` makes and it is made here explicitly:
/// `hadris-iso` 2.2.0's own ranking happens to agree today, and its
/// documentation says to select the namespace rather than rely on that.
///
/// Never `IsoImage::root_dir`, which calls `RootDirs::best_choice`, which
/// panics on an image with no directory tree at all.
///
fn root_of<D: IsoRead + IsoSeek>(image: &IsoImage<D>) -> Result<RootDir> {
    if image.supports_rrip()
        && let Some(primary) = image
            .root_dirs()
            .iter()
            .find(|root| matches!(root.entry_type(), EntryType::Level1 { .. }))
    {
        return Ok(*primary);
    }
    image
        .root_dirs()
        .try_best_choice()
        .ok_or_else(|| Error::msg("the ISO 9660 volume has no directory tree"))
}

/// One member of an ISO, as the index holds it.
///
/// The locator is the interesting part. A single-extent member is a
/// contiguous byte range of the container, so it becomes
/// [`Locator::Offset`] with `data` **relative to the region**, and reading it
/// is a seek and a copy with no directory walk at all, which is what makes a
/// 4 GiB member of a 40 GiB ISO cost nothing to open. A multi-extent member
/// becomes [`Locator::None`] and is read by walking to it again.
///
/// One parameter more than the design spells: `name`, the
/// decoded name, because the caller needs the same name to build the child
/// directory's prefix and decoding it twice would be both wasted work and a
/// chance for the two answers to disagree.
///
/// Returns false when indexing should stop.
fn push_entry(
    sink: &mut dyn IndexSink,
    parent: &str,
    name: &str,
    entry: &DirEntry,
    region: &Region,
) -> bool {
    let header = entry.header();
    // Rule 6. `hadris-iso` raises this itself before the record reaches here,
    // so this is the check that keeps being right if it ever stops doing so:
    // an interleaved member's bytes are not contiguous and the locator this
    // module builds says they are.
    if header.file_unit_size != 0 || header.interleave_gap_size != 0 {
        return true;
    }

    let rrip = entry.rrip.as_ref();
    let (mode, uid, gid) = match rrip.and_then(|meta| meta.posix_attributes) {
        Some(px) => (
            untrusted_mode(px.file_mode.read()),
            px.file_uid.read(),
            px.file_gid.read(),
        ),
        None => (0, 0, 0),
    };

    let kind = if let Some(target) = rrip.and_then(|meta| meta.symlink_target.clone()) {
        MemberKind::Symlink(target)
    } else if entry.is_directory() {
        MemberKind::Dir
    } else {
        MemberKind::File
    };

    let locator = match kind {
        MemberKind::File => match extent_of(entry, region) {
            Some(locator) => locator,
            // Rule 5: the record addresses bytes that are not in this volume.
            // Refused here, at index time, so the member never exists and can
            // therefore never be listed, opened or extracted.
            None => return true,
        },
        MemberKind::Dir | MemberKind::Symlink(_) | MemberKind::Hardlink(_) | MemberKind::Other => {
            Locator::None
        }
    };

    let size = match kind {
        MemberKind::File => entry.total_size(),
        MemberKind::Dir | MemberKind::Symlink(_) | MemberKind::Hardlink(_) | MemberKind::Other => 0,
    };

    sink.push(RawMember {
        name: join(parent, name),
        kind,
        size,
        mtime: member_time(entry),
        mode,
        uid,
        gid,
        locator,
    })
}

/// Where a file member's bytes are, relative to `region`, or `None` when the
/// record addresses bytes the region does not have.
///
/// A single extent becomes a range. More than one - a member over 4 GiB, so
/// vanishingly rare - becomes [`Locator::None`], because a locator names one
/// range and this member is several; every one of them is still checked here,
/// so a multi-extent member that lies is refused at index time like any other.
fn extent_of(entry: &DirEntry, region: &Region) -> Option<Locator> {
    let mut extents = entry.extents();
    let first = extents.next()?;
    let at = byte_offset(first.sector.0)?;
    let len = u64::from(first.length);
    if !region.contains(at, len) {
        return None;
    }
    let mut multi = false;
    for extent in extents {
        multi = true;
        let at = byte_offset(extent.sector.0)?;
        if !region.contains(at, u64::from(extent.length)) {
            return None;
        }
    }
    if multi {
        Some(Locator::None)
    } else {
        Some(Locator::Offset { data: at, len })
    }
}

/// A logical sector number as a byte offset, or `None` when it does not fit.
///
/// Saturating arithmetic would answer a number here, and a number is what a
/// caller would then read from; an overflow is a record that cannot be
/// honest, so it has no offset at all.
fn byte_offset(sector: usize) -> Option<u64> {
    u64::try_from(sector).ok()?.checked_mul(LOGICAL_BLOCK)
}

/// Whether a directory's own extent lies inside the region.
///
/// The same check as [`extent_of`], for the thing a walk descends into rather
/// than the thing it lists (the design I4).
fn dir_within(region: &Region, dir: &DirectoryRef) -> bool {
    match (byte_offset(dir.extent.0), u64::try_from(dir.size)) {
        (Some(at), Ok(len)) => region.contains(at, len),
        _ => false,
    }
}

/// `parent/name`, or `name` at the volume's root.
fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

/// The name a member is listed under.
///
/// Rule 2: the Rock Ridge `NM` alternate name where there is one, else the
/// identifier in the record, decoded for the namespace the tree is in.
fn member_name(entry: &DirEntry, names: NameSource) -> String {
    if let Some(rrip) = entry.rrip.as_ref()
        && let Some(alternate) = rrip.alternate_name.as_deref()
    {
        return alternate.trim_end_matches('\0').to_string();
    }
    match names {
        NameSource::Joliet => strip_version(&entry.record.joliet_name()),
        NameSource::Iso => strip_trailing_dot(&strip_version(&String::from_utf8_lossy(
            entry.record.name(),
        ))),
    }
}

/// A name with its trailing NULs and its `;1` version suffix removed.
///
/// The suffix is removed only when what follows the last `;` is a non-empty
/// run of digits, so a file genuinely called `a;b` keeps its name.
fn strip_version(name: &str) -> String {
    let trimmed = name.trim_end_matches('\0');
    match trimmed.rsplit_once(';') {
        Some((base, version))
            if !version.is_empty() && version.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            base.to_string()
        }
        _ => trimmed.to_string(),
    }
}

/// An ISO 9660 identifier's trailing `.`, removed.
///
/// ISO 9660 stores a name with no extension as `BOOT.`, which the kernel's
/// `isofs` shows as `BOOT` and so does this. Not applied to a Joliet
/// identifier, which is a real name and may end in a dot on purpose. A name
/// that is nothing but dots is left alone and is refused by
/// [`crate::vfs::is_plain_name`] afterwards.
fn strip_trailing_dot(name: &str) -> String {
    match name.strip_suffix('.') {
        Some(stripped) if !stripped.is_empty() && stripped != "." => stripped.to_string(),
        _ => name.to_string(),
    }
}

/// A member's modification time, where the image records one.
///
/// The Rock Ridge `TF` modify stamp when there is one, else the seven bytes
/// the directory record carries itself. Those seven bytes are reached through
/// `DirectoryRecordHeader::to_bytes`, because `hadris-iso` 2.2.0 keeps
/// `DirDateTime`'s fields private and offers no accessor for them; the offset
/// is fixed by ECMA-119 and by the header's own `#[repr(C)]` layout.
///
/// A stamp that is out of range yields `None` rather than an error: a wrong
/// mtime is not worth failing a listing over.
fn member_time(entry: &DirEntry) -> Option<SystemTime> {
    if let Some(rrip) = entry.rrip.as_ref()
        && let Some(stamps) = rrip.timestamps.as_ref()
        && let Some(modify) = stamps.modify
    {
        return civil_to_system(
            i32::from(modify.year),
            modify.month,
            modify.day,
            modify.hour,
            modify.minute,
            modify.second,
            modify.gmt_offset,
        );
    }
    let header = entry.header().to_bytes();
    let stamp = header.get(DATE_TIME_AT..DATE_TIME_AT.saturating_add(7))?;
    civil_to_system(
        1900i32.checked_add(i32::from(*stamp.first()?))?,
        *stamp.get(1)?,
        *stamp.get(2)?,
        *stamp.get(3)?,
        *stamp.get(4)?,
        *stamp.get(5)?,
        i8::from_ne_bytes([*stamp.get(6)?]),
    )
}

/// Where the recording date sits in a directory record header.
///
/// `len`, `extended_attr_record`, the two eight-byte both-endian numbers, and
/// then the seven-byte date. ECMA-119 §9.1.5.
const DATE_TIME_AT: usize = 18;

/// A civil date, with a GMT offset in fifteen-minute intervals, as a
/// [`SystemTime`].
fn civil_to_system(
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    offset_quarters: i8,
) -> Option<SystemTime> {
    let date = chrono::NaiveDate::from_ymd_opt(year, u32::from(month), u32::from(day))?;
    let time = date.and_hms_opt(u32::from(hour), u32::from(minute), u32::from(second))?;
    let seconds = time
        .and_utc()
        .timestamp()
        .checked_sub(i64::from(offset_quarters).checked_mul(15 * 60)?)?;
    match u64::try_from(seconds) {
        Ok(after) => UNIX_EPOCH.checked_add(Duration::from_secs(after)),
        Err(_) => {
            let before = seconds.checked_neg().and_then(|n| u64::try_from(n).ok())?;
            UNIX_EPOCH.checked_sub(Duration::from_secs(before))
        }
    }
}

/// Read a member whose bytes are not one contiguous range, by walking to it.
///
/// The walk is by component and by decoded name, using the same decoding the
/// index used, so the two cannot disagree about which record a path names.
fn read_by_walking(region: &Region, member: &Member, out: &mut dyn Write) -> Result<u64> {
    let image = open_image(region)?;
    let root = root_of(&image)?;
    let names = NameSource::of(root.entry_type());
    let entry = find_entry(&image, root, names, &member.path)?
        .ok_or_else(|| Error::NotFound(member.path.clone()))?;
    let header = entry.header();
    if header.file_unit_size != 0 || header.interleave_gap_size != 0 {
        return Err(Error::msg(format!(
            "{}: this file is stored interleaved and cannot be read",
            member.path
        )));
    }
    if entry.is_directory() {
        return Err(Error::Unsupported("reading a directory as a file"));
    }
    let mut written = 0u64;
    for extent in entry.extents() {
        let Some(at) = byte_offset(extent.sector.0) else {
            return Err(out_of_volume(&member.path));
        };
        let len = u64::from(extent.length);
        if !region.contains(at, len) {
            return Err(out_of_volume(&member.path));
        }
        written = written.saturating_add(copy_range(region, at, len, out)?);
    }
    Ok(written)
}

/// The one message for a record that addresses bytes this volume does not
/// have.
fn out_of_volume(path: &str) -> Error {
    Error::msg(format!(
        "{path}: this file is recorded outside the volume it is in"
    ))
}

/// The entry `path` names, found one component at a time.
///
/// Never by handing a whole path to the library: `IsoImage::find_path` calls
/// `root_dir`, which panics on an image with no directory tree
/// (the design I9), and it matches names by its own rules rather
/// than by the ones the index was built with.
fn find_entry<D: IsoRead + IsoSeek>(
    image: &IsoImage<D>,
    root: RootDir,
    names: NameSource,
    path: &str,
) -> Result<Option<DirEntry>> {
    let mut here = root.dir_ref();
    let mut components = path.split('/').filter(|part| !part.is_empty()).peekable();
    let mut depth = 0usize;
    while let Some(component) = components.next() {
        depth = depth.saturating_add(1);
        if depth > MAX_DIR_DEPTH {
            return Ok(None);
        }
        let mut found = None;
        for item in image.open_dir(here).entries() {
            let Ok(entry) = item else {
                break;
            };
            if entry.is_special() {
                continue;
            }
            if member_name(&entry, names) == component {
                found = Some(entry);
                break;
            }
        }
        let Some(entry) = found else {
            return Ok(None);
        };
        if components.peek().is_none() {
            return Ok(Some(entry));
        }
        if !entry.is_directory() {
            return Ok(None);
        }
        here = entry
            .as_dir_ref(image)
            .map_err(|e| Error::msg(format!("the ISO 9660 volume is damaged: {e}")))?;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One ISO 9660 logical block.
    const SECTOR: usize = LOGICAL_BLOCK as usize;

    /// A sink that keeps what it was given, so a test can assert on it.
    #[derive(Debug, Default)]
    struct Sink {
        members: Vec<RawMember>,
        stop_after: Option<usize>,
    }

    impl Sink {
        fn names(&self) -> Vec<String> {
            self.members.iter().map(|m| m.name.clone()).collect()
        }

        fn get(&self, name: &str) -> &RawMember {
            self.members
                .iter()
                .find(|m| m.name == name)
                .expect("the listing has that member")
        }
    }

    impl IndexSink for Sink {
        fn push(&mut self, raw: RawMember) -> bool {
            self.members.push(raw);
            match self.stop_after {
                Some(stop) => self.members.len() < stop,
                None => true,
            }
        }

        fn cancelled(&self) -> bool {
            false
        }
    }

    fn temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hcmd-iso-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// Write a both-endian 32-bit number: little endian, then big endian.
    fn both32(out: &mut [u8], at: usize, value: u32) {
        out[at..at + 4].copy_from_slice(&value.to_le_bytes());
        out[at + 4..at + 8].copy_from_slice(&value.to_be_bytes());
    }

    /// Write a both-endian 16-bit number.
    fn both16(out: &mut [u8], at: usize, value: u16) {
        out[at..at + 2].copy_from_slice(&value.to_le_bytes());
        out[at + 2..at + 4].copy_from_slice(&value.to_be_bytes());
    }

    /// The flag bit that marks a directory record as a directory.
    const IS_DIR: u8 = 0x02;

    /// The flag bit that says this record is one extent of a member whose
    /// bytes continue in the next record.
    const NOT_FINAL: u8 = 0x80;

    /// One directory record: the 33-byte header, the identifier, and the pad
    /// byte an even identifier needs so that every record starts on an even
    /// offset (ECMA-119 9.1).
    fn record(identifier: &[u8], extent: u32, len: u32, flags: u8) -> Vec<u8> {
        let mut size = 33 + identifier.len();
        if identifier.len().is_multiple_of(2) {
            size += 1;
        }
        let mut out = vec![0u8; size];
        out[0] = u8::try_from(size).expect("a record shorter than 256 bytes");
        both32(&mut out, 2, extent);
        both32(&mut out, 10, len);
        // The recording date: 2024-02-29 12:34:56 UTC, which is a leap day, so
        // a test that reads it back proves the conversion rather than the
        // calendar being forgiving.
        out[18] = 124;
        out[19] = 2;
        out[20] = 29;
        out[21] = 12;
        out[22] = 34;
        out[23] = 56;
        out[24] = 0;
        out[25] = flags;
        both16(&mut out, 28, 1);
        out[32] = u8::try_from(identifier.len()).expect("a short identifier");
        out[33..33 + identifier.len()].copy_from_slice(identifier);
        out
    }

    /// A minimal ISO 9660 image, built byte by byte.
    ///
    /// Twenty-two sectors: sixteen of system area, a primary volume
    /// descriptor, a terminator, the root directory, one subdirectory and two
    /// files. No Joliet, no Rock Ridge and no path table, which is the point:
    /// it is the smallest thing this reader has to cope with, and every field
    /// in it is one this test chose.
    fn minimal_iso() -> Vec<u8> {
        minimal_iso_named(b"HELLO.TXT;1")
    }

    /// [`minimal_iso`] with the root's one file record given the identifier
    /// the caller wants, so a test can put a name in an image that no
    /// filesystem would accept out of one.
    fn minimal_iso_named(identifier: &[u8]) -> Vec<u8> {
        const TOTAL: usize = 22;
        const ROOT: u32 = 18;
        const SUB: u32 = 19;
        const HELLO: u32 = 20;
        const INNER: u32 = 21;

        let mut image = vec![0u8; TOTAL * SECTOR];

        // The primary volume descriptor, at sector 16.
        let pvd = 16 * SECTOR;
        image[pvd] = 1;
        image[pvd + 1..pvd + 6].copy_from_slice(b"CD001");
        image[pvd + 6] = 1;
        image[pvd + 8..pvd + 40].fill(b' ');
        image[pvd + 40..pvd + 72].fill(b' ');
        image[pvd + 40..pvd + 49].copy_from_slice(b"HANDBUILT");
        both32(&mut image, pvd + 80, u32::try_from(TOTAL).expect("small"));
        both16(&mut image, pvd + 120, 1);
        both16(&mut image, pvd + 124, 1);
        both16(&mut image, pvd + 128, 2048);
        both32(&mut image, pvd + 132, 0);
        let root_record = record(&[0x00], ROOT, u32::try_from(SECTOR).expect("small"), IS_DIR);
        image[pvd + 156..pvd + 156 + root_record.len()].copy_from_slice(&root_record);
        image[pvd + 190..pvd + 813].fill(b' ');
        image[pvd + 813..pvd + 881].fill(b'0');
        image[pvd + 881] = 1;

        // The volume descriptor set terminator, at sector 17.
        let end = 17 * SECTOR;
        image[end] = 0xFF;
        image[end + 1..end + 6].copy_from_slice(b"CD001");
        image[end + 6] = 1;

        // The root directory, at sector 18.
        let sector_len = u32::try_from(SECTOR).expect("small");
        let mut root = Vec::new();
        root.extend_from_slice(&record(&[0x00], ROOT, sector_len, IS_DIR));
        root.extend_from_slice(&record(&[0x01], ROOT, sector_len, IS_DIR));
        root.extend_from_slice(&record(b"SUB", SUB, sector_len, IS_DIR));
        root.extend_from_slice(&record(identifier, HELLO, 11, 0));
        let at = ROOT as usize * SECTOR;
        image[at..at + root.len()].copy_from_slice(&root);

        // One subdirectory, at sector 19.
        let mut sub = Vec::new();
        sub.extend_from_slice(&record(&[0x00], SUB, sector_len, IS_DIR));
        sub.extend_from_slice(&record(&[0x01], ROOT, sector_len, IS_DIR));
        sub.extend_from_slice(&record(b"INNER.TXT;1", INNER, 5, 0));
        let at = SUB as usize * SECTOR;
        image[at..at + sub.len()].copy_from_slice(&sub);

        let at = HELLO as usize * SECTOR;
        image[at..at + 11].copy_from_slice(b"hello there");
        let at = INNER as usize * SECTOR;
        image[at..at + 5].copy_from_slice(b"inner");

        image
    }

    /// A minimal ISO whose only member is recorded in two extents, which is
    /// what an ISO 9660 file larger than 4 GiB looks like without needing one.
    ///
    /// Two directory records share an identifier; the first carries
    /// `NOT_FINAL` and the second finishes the member. The whole point of the
    /// fixture is that such a member has no single byte range, so the index
    /// gives it no locator and reading it walks back to it.
    fn multi_extent_iso() -> Vec<u8> {
        const TOTAL: usize = 22;
        const ROOT: u32 = 18;
        const FIRST: u32 = 20;
        const SECOND: u32 = 21;

        let mut image = vec![0u8; TOTAL * SECTOR];
        let pvd = 16 * SECTOR;
        image[pvd] = 1;
        image[pvd + 1..pvd + 6].copy_from_slice(b"CD001");
        image[pvd + 6] = 1;
        image[pvd + 8..pvd + 40].fill(b' ');
        image[pvd + 40..pvd + 72].fill(b' ');
        image[pvd + 40..pvd + 45].copy_from_slice(b"SPLIT");
        both32(&mut image, pvd + 80, u32::try_from(TOTAL).expect("small"));
        both16(&mut image, pvd + 120, 1);
        both16(&mut image, pvd + 124, 1);
        both16(&mut image, pvd + 128, 2048);
        both32(&mut image, pvd + 132, 0);
        let sector_len = u32::try_from(SECTOR).expect("small");
        let root_record = record(&[0x00], ROOT, sector_len, IS_DIR);
        image[pvd + 156..pvd + 156 + root_record.len()].copy_from_slice(&root_record);
        image[pvd + 190..pvd + 813].fill(b' ');
        image[pvd + 813..pvd + 881].fill(b'0');
        image[pvd + 881] = 1;

        let end = 17 * SECTOR;
        image[end] = 0xFF;
        image[end + 1..end + 6].copy_from_slice(b"CD001");
        image[end + 6] = 1;

        let mut root = Vec::new();
        root.extend_from_slice(&record(&[0x00], ROOT, sector_len, IS_DIR));
        root.extend_from_slice(&record(&[0x01], ROOT, sector_len, IS_DIR));
        root.extend_from_slice(&record(b"BIG.BIN;1", FIRST, sector_len, NOT_FINAL));
        root.extend_from_slice(&record(b"BIG.BIN;1", SECOND, 100, 0));
        let at = ROOT as usize * SECTOR;
        image[at..at + root.len()].copy_from_slice(&root);

        let at = FIRST as usize * SECTOR;
        image[at..at + SECTOR].fill(0xAA);
        let at = SECOND as usize * SECTOR;
        image[at..at + 100].fill(0xBB);

        image
    }

    fn write_image(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = temp(tag).join("image.iso");
        std::fs::write(&path, bytes).expect("write the fixture");
        path
    }

    fn index_of(path: &std::path::Path) -> Sink {
        let region = Region::whole(path).expect("open the image");
        let mut sink = Sink::default();
        Iso9660.index(&region, &mut sink).expect("index the image");
        sink
    }

    /// Index an image that cannot be read to the end: the listing that was
    /// produced, and the reason the rest is missing.
    fn index_partial(path: &std::path::Path) -> (Sink, String) {
        let region = Region::whole(path).expect("open the image");
        let mut sink = Sink::default();
        let why = Iso9660
            .index(&region, &mut sink)
            .expect_err("this image cannot be read to the end");
        (sink, why.to_string())
    }

    fn member_of(raw: &RawMember) -> Member {
        Member {
            path: raw.name.clone(),
            kind: raw.kind.clone(),
            size: raw.size,
            mtime: raw.mtime,
            mode: raw.mode,
            uid: raw.uid,
            gid: raw.gid,
            locator: raw.locator,
            synthetic: false,
        }
    }

    fn read_of(path: &std::path::Path, raw: &RawMember) -> Vec<u8> {
        let region = Region::whole(path).expect("open the image");
        let mut out = Vec::new();
        Iso9660
            .read_member(&region, &member_of(raw), &mut out)
            .expect("read the member");
        out
    }

    #[test]
    fn a_hand_built_iso_lists_its_members() {
        let path = write_image("list", &minimal_iso());
        let sink = index_of(&path);
        let mut names = sink.names();
        names.sort();
        assert_eq!(
            names,
            vec![
                "HELLO.TXT".to_string(),
                "SUB".to_string(),
                "SUB/INNER.TXT".to_string(),
            ]
        );
    }

    #[test]
    fn dot_and_dotdot_are_absent_from_the_listing() {
        // Every ISO directory record begins with a `.` and a `..` entry, so
        // the count comes first: an indexer that produced nothing at all would
        // run the loop below zero times and look like one that filtered them.
        let path = write_image("dots", &minimal_iso());
        let names = index_of(&path).names();
        assert_eq!(
            names.len(),
            3,
            "the three members of the fixture: {names:?}"
        );
        for name in &names {
            for part in name.split('/') {
                assert_ne!(part, ".");
                assert_ne!(part, "..");
                assert!(!part.is_empty());
            }
        }
    }

    #[test]
    fn a_member_reads_back_the_bytes_that_were_put_in_it() {
        let path = write_image("read", &minimal_iso());
        let sink = index_of(&path);
        let hello = sink.get("HELLO.TXT");
        assert_eq!(hello.kind, MemberKind::File);
        assert_eq!(hello.size, 11);
        assert!(matches!(hello.locator, Locator::Offset { len: 11, .. }));
        assert_eq!(read_of(&path, hello), b"hello there".to_vec());
        assert_eq!(read_of(&path, sink.get("SUB/INNER.TXT")), b"inner".to_vec());
    }

    #[test]
    fn a_directory_carries_no_bytes_and_no_locator() {
        let path = write_image("dir", &minimal_iso());
        let sink = index_of(&path);
        let sub = sink.get("SUB");
        assert_eq!(sub.kind, MemberKind::Dir);
        assert_eq!(sub.size, 0);
        assert_eq!(sub.locator, Locator::None);
    }

    #[test]
    fn an_iso_without_rock_ridge_reports_no_posix_metadata() {
        let path = write_image("posix", &minimal_iso());
        let sink = index_of(&path);
        let hello = sink.get("HELLO.TXT");
        assert_eq!(hello.mode, 0, "a zero mode is the honest answer");
        assert_eq!(hello.uid, 0);
        assert_eq!(hello.gid, 0);
    }

    #[test]
    fn the_record_s_own_recording_date_is_the_mtime() {
        let path = write_image("mtime", &minimal_iso());
        let sink = index_of(&path);
        let at = sink
            .get("HELLO.TXT")
            .mtime
            .expect("the record carries a date")
            .duration_since(UNIX_EPOCH)
            .expect("after the epoch")
            .as_secs();
        // 2024-02-29 12:34:56 UTC.
        assert_eq!(at, 1_709_210_096);
    }

    #[test]
    fn a_single_extent_member_is_offered_as_a_seekable_window() {
        use std::io::{Read as _, SeekFrom};
        let path = write_image("seek", &minimal_iso());
        let sink = index_of(&path);
        let region = Region::whole(&path).expect("open the image");
        let mut handle = Iso9660
            .open_member(&region, &member_of(sink.get("HELLO.TXT")))
            .expect("no failure")
            .expect("an ISO member is seekable");
        std::io::Seek::seek(&mut handle, SeekFrom::Start(6)).expect("seek");
        let mut tail = Vec::new();
        handle.read_to_end(&mut tail).expect("read");
        assert_eq!(tail, b"there".to_vec());
    }

    #[test]
    fn the_volume_label_is_the_one_in_the_descriptor() {
        let path = write_image("label", &minimal_iso());
        let region = Region::whole(&path).expect("open the image");
        assert_eq!(Iso9660.volume_label(&region).as_deref(), Some("HANDBUILT"));
    }

    #[test]
    fn an_extent_past_the_end_of_the_image_is_refused_and_the_rest_lists() {
        let mut bytes = minimal_iso();
        // The `HELLO.TXT;1` record is the fourth in the root directory.
        let at = 18 * SECTOR + 34 + 34 + 36;
        both32(&mut bytes, at + 2, 4_000_000);
        let path = write_image("extent", &bytes);
        let names = index_of(&path).names();
        assert!(!names.contains(&"HELLO.TXT".to_string()), "{names:?}");
        assert!(names.contains(&"SUB".to_string()), "{names:?}");
        assert!(names.contains(&"SUB/INNER.TXT".to_string()), "{names:?}");
    }

    #[test]
    fn a_declared_length_past_the_end_of_the_image_is_refused() {
        let mut bytes = minimal_iso();
        let at = 18 * SECTOR + 34 + 34 + 36;
        both32(&mut bytes, at + 10, u32::MAX);
        let path = write_image("length", &bytes);
        let names = index_of(&path).names();
        assert!(!names.contains(&"HELLO.TXT".to_string()), "{names:?}");
        assert!(names.contains(&"SUB".to_string()), "{names:?}");
    }

    #[test]
    fn a_directory_recorded_outside_the_volume_is_named_rather_than_shown_empty() {
        let mut bytes = minimal_iso();
        // `SUB` is the third record in the root directory. A directory that is
        // listed and never walked would look like an empty one, so the
        // listing says why instead.
        let at = 18 * SECTOR + 34 + 34;
        both32(&mut bytes, at + 2, 4_000_000);
        let path = write_image("lostdir", &bytes);
        let (sink, why) = index_partial(&path);
        let names = sink.names();
        assert!(names.contains(&"HELLO.TXT".to_string()), "{names:?}");
        assert!(!names.contains(&"SUB/INNER.TXT".to_string()), "{names:?}");
        assert!(why.contains("SUB"), "{why}");
        assert!(why.contains("outside the volume"), "{why}");
    }

    #[test]
    fn a_directory_that_points_back_at_the_root_terminates() {
        let mut bytes = minimal_iso();
        // `SUB` is the third record in the root directory; point it at the
        // root's own extent, which is the cheapest cycle an image can have.
        let at = 18 * SECTOR + 34 + 34;
        both32(&mut bytes, at + 2, 18);
        let path = write_image("cycle", &bytes);
        let names = index_of(&path).names();
        assert_eq!(names.iter().filter(|n| n.as_str() == "SUB").count(), 1);
        assert!(names.len() < 20, "{names:?}");
    }

    #[test]
    fn a_member_whose_record_declares_interleaving_is_not_listed() {
        let mut bytes = minimal_iso();
        let at = 18 * SECTOR + 34 + 34 + 36;
        bytes[at + 26] = 4;
        bytes[at + 27] = 4;
        let path = write_image("interleave", &bytes);
        let (sink, why) = index_partial(&path);
        let names = sink.names();
        assert!(!names.contains(&"HELLO.TXT".to_string()), "{names:?}");
        // The rest of the directory, and the directory below it, still list:
        // one member is missing, not the image.
        assert!(names.contains(&"SUB".to_string()), "{names:?}");
        assert!(names.contains(&"SUB/INNER.TXT".to_string()), "{names:?}");
        assert!(why.contains("cannot be read"), "{why}");
    }

    #[test]
    fn a_sink_that_says_stop_is_obeyed_promptly() {
        let path = write_image("stop", &minimal_iso());
        let region = Region::whole(&path).expect("open the image");
        let mut sink = Sink {
            stop_after: Some(1),
            ..Sink::default()
        };
        Iso9660.index(&region, &mut sink).expect("index");
        assert_eq!(sink.members.len(), 1);
    }

    #[test]
    fn a_file_that_is_not_an_iso_fails_rather_than_listing_nothing() {
        let path = temp("notiso").join("notes.img");
        std::fs::write(&path, vec![b'q'; 100_000]).expect("write");
        let region = Region::whole(&path).expect("open");
        let mut sink = Sink::default();
        let err = Iso9660
            .index(&region, &mut sink)
            .expect_err("not an ISO 9660 volume");
        assert!(err.to_string().contains("damaged"), "{err}");
        assert!(sink.members.is_empty());
    }

    #[test]
    fn a_locator_outside_the_volume_produces_no_bytes_at_all() {
        let path = write_image("outside", &minimal_iso());
        let sink = index_of(&path);
        let mut member = member_of(sink.get("HELLO.TXT"));
        member.locator = Locator::Offset {
            data: u64::MAX - 8,
            len: 64,
        };
        let region = Region::whole(&path).expect("open the image");
        let mut out = Vec::new();
        let err = Iso9660
            .read_member(&region, &member, &mut out)
            .expect_err("a read outside the volume is refused");
        assert!(out.is_empty(), "{err}");
    }

    #[test]
    fn a_version_suffix_is_stripped_and_a_semicolon_that_is_not_one_is_kept() {
        assert_eq!(strip_version("HELLO.TXT;1"), "HELLO.TXT");
        assert_eq!(strip_version("HELLO.TXT;42"), "HELLO.TXT");
        assert_eq!(strip_version("weird;name"), "weird;name");
        assert_eq!(strip_version("trailing;"), "trailing;");
        assert_eq!(strip_version("nul\0\0"), "nul");
    }

    #[test]
    fn an_iso_identifier_loses_its_trailing_dot_and_a_bare_dot_does_not() {
        assert_eq!(strip_trailing_dot("BOOT."), "BOOT");
        assert_eq!(strip_trailing_dot("BOOT.TXT"), "BOOT.TXT");
        assert_eq!(strip_trailing_dot("."), ".");
        assert_eq!(strip_trailing_dot(".."), "..");
    }

    #[test]
    fn a_sector_number_that_cannot_be_a_byte_offset_has_none() {
        assert_eq!(byte_offset(0), Some(0));
        assert_eq!(byte_offset(18), Some(36_864));
        assert_eq!(byte_offset(usize::MAX), None);
    }

    #[test]
    fn a_member_recorded_in_two_extents_has_no_locator_and_reads_whole() {
        let path = write_image("multi", &multi_extent_iso());
        let sink = index_of(&path);
        let big = sink.get("BIG.BIN");
        assert_eq!(big.size, 2148);
        assert_eq!(
            big.locator,
            Locator::None,
            "a member with two extents has no single byte range"
        );
        let mut want = vec![0xAAu8; SECTOR];
        want.extend_from_slice(&[0xBBu8; 100]);
        assert_eq!(read_of(&path, big), want);
    }

    #[test]
    fn a_member_recorded_in_two_extents_is_not_offered_as_a_window() {
        let path = write_image("multiwindow", &multi_extent_iso());
        let sink = index_of(&path);
        let region = Region::whole(&path).expect("open the image");
        let handle = Iso9660
            .open_member(&region, &member_of(sink.get("BIG.BIN")))
            .expect("no failure");
        assert!(
            handle.is_none(),
            "there is no contiguous window to hand out"
        );
    }

    #[test]
    fn an_identifier_carrying_a_separator_never_reaches_the_listing() {
        let mut bytes = minimal_iso();
        // `HELLO.TXT;1` is the fourth record in the root directory. One byte
        // turns its name into a path, which is the shape of the Zip Slip
        // class of bug applied to an image.
        let at = 18 * SECTOR + 34 + 34 + 36 + 33;
        bytes[at + 5] = b'/';
        let path = write_image("separator", &bytes);
        let names = index_of(&path).names();
        assert!(!names.iter().any(|n| n.contains("HELLO")), "{names:?}");
        assert!(names.contains(&"SUB".to_string()), "{names:?}");
    }

    #[test]
    fn an_identifier_that_spells_the_parent_directory_is_refused() {
        // A record that is not one of the two special ones, whose identifier
        // is nonetheless `..`. Rock Ridge can spell the same thing with an
        // `NM` entry in a record that is not special either, which is why the
        // decoded name is checked and not only the record's identity.
        let path = write_image("parent", &minimal_iso_named(b".."));
        let names = index_of(&path).names();
        assert!(!names.iter().any(|n| n.contains("..")), "{names:?}");
        assert!(names.contains(&"SUB".to_string()), "{names:?}");
    }

    #[test]
    fn an_identifier_that_is_a_single_dot_is_refused_too() {
        let path = write_image("current", &minimal_iso_named(b"."));
        let names = index_of(&path).names();
        assert!(!names.iter().any(|n| n.ends_with('.')), "{names:?}");
        assert!(names.contains(&"SUB".to_string()), "{names:?}");
    }

    #[test]
    fn an_identifier_that_is_an_absolute_path_never_reaches_the_listing() {
        let path = write_image("absolute", &minimal_iso_named(b"/etc/passwd"));
        let names = index_of(&path).names();
        assert!(!names.iter().any(|n| n.contains("passwd")), "{names:?}");
        assert!(names.contains(&"SUB".to_string()), "{names:?}");
    }

    #[test]
    fn a_directory_cannot_be_read_as_a_file() {
        let path = write_image("readdir", &minimal_iso());
        let sink = index_of(&path);
        let region = Region::whole(&path).expect("open the image");
        let mut out = Vec::new();
        let err = Iso9660
            .read_member(&region, &member_of(sink.get("SUB")), &mut out)
            .expect_err("a directory has no contents");
        assert!(out.is_empty(), "{err}");
    }

    #[test]
    fn a_member_that_is_not_in_the_image_is_not_found() {
        let path = write_image("missing", &minimal_iso());
        let region = Region::whole(&path).expect("open the image");
        let member = Member {
            path: "NOPE.TXT".to_string(),
            kind: MemberKind::File,
            size: 4,
            mtime: None,
            mode: 0,
            uid: 0,
            gid: 0,
            locator: Locator::None,
            synthetic: false,
        };
        let mut out = Vec::new();
        let err = Iso9660
            .read_member(&region, &member, &mut out)
            .expect_err("nothing of that name");
        assert!(matches!(err, Error::NotFound(_)), "{err}");
    }

    #[test]
    fn the_reader_is_iso_9660_and_says_it_cannot_be_written() {
        assert_eq!(Iso9660.id(), FsId::Iso9660);
        let caps = Iso9660.capabilities();
        assert!(!caps.writable);
        assert!(caps.seekable);
        assert!(caps.random_access);
        assert!(caps.has_directories);
        assert!(!caps.atomic_rename);
        assert!(!caps.paged_listing);
        assert!(!caps.can_execute);
    }
}
