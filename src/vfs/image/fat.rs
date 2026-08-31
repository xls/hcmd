//! FAT12, FAT16 and FAT32, on `fatfs`.
//!
//! The three are one format with three table widths, so they are one reader.
//! Which of them a volume is cannot be decided from a signature at all - it is
//! arithmetic over the BPB's cluster count - so the superblock sniffer answers
//! the family and [`fat_id`] answers the volume, by opening it.
//!
//! # What FAT does not have
//!
//! FAT has **no POSIX mode bits, no owner, no group and no symbolic links**.
//! That is not a limitation of this reader and it is not something to be
//! filled in later: the on-disk directory entry is 32 bytes of name,
//! attributes, timestamps, a starting cluster and a size, and there is nowhere
//! for a mode to live. So [`Fat::capabilities`] says what a FAT volume really
//! offers, `mode`, `uid` and `gid` are left at `0` rather than invented, and
//! no member is ever a [`MemberKind::Symlink`]. the design asks `Capabilities`
//! not to assume POSIX, and a `0o755` produced because a file is a file would
//! be a lie the panel's attribute column repeats on every row.
//!
//! # Nothing is held open
//!
//! `fatfs::FileSystem` contains a `RefCell` and is therefore not `Sync`, while
//! `Vfs` is `Send + Sync`. A volume is opened, used and dropped inside one
//! call, always (the design I11, J14). Reading a member is a fresh
//! open and a fresh walk down the directory chain, which is a handful of
//! 4 KiB block reads and is bounded by [`MAX_DIR_DEPTH`].
//!
//! # Read-only, twice over
//!
//! `fatfs::FileSystem<T>` is bounded on `T: Read + Write + Seek` and 0.3.6 has
//! no read-only constructor. [`super::block::Reader`] satisfies the bound and
//! refuses every write with `PermissionDenied` without touching the file, and
//! the file underneath it is opened `O_RDONLY`, so the kernel would refuse the
//! write even if the refusal here were removed.
//!

use std::io::{Read as _, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{NaiveDate, NaiveTime, TimeZone as _};

use crate::error::{Error, Result};
use crate::vfs::archive::index::{IndexSink, Locator, Member, MemberKind, RawMember};
use crate::vfs::{Capabilities, LatencyClass, PlainName, is_plain_name};

use super::block::{Reader, Region};
use super::format::{FsId, MAX_DIR_DEPTH, VolumeFormat};

/// The most directory entries one directory of a FAT volume yields before the
/// walk gives up on it.
///
/// A cluster chain is a linked list and nothing in the format says it has to
/// end: a crafted FAT whose directory cluster points at itself yields the same
/// entries for ever, and `fatfs` follows the chain it is given without
/// checking. The index's own bounds do not catch this one, because every
/// repetition has a name the index already holds and so replaces rather than
/// grows.
///
/// 65 536 is the FAT specification's own ceiling on a directory: 2 MiB of
/// 32-byte entries. A directory that yields more than that is not a directory
/// this reader can believe.
pub const MAX_DIR_ENTRIES: usize = 65_536;

/// The buffer one member's bytes are copied through.
///
/// Nothing here is proportional to the member, the volume or the image
/// (the design I6): a member larger than memory copies through
/// this and no more.
const COPY_BUFFER: usize = 64 * 1024;

/// The FAT reader.
///
/// Zero-sized and `'static`, like every `ArchiveFormat`: everything stateful
/// belongs to the backend and the session.
#[derive(Debug)]
pub struct Fat;

impl VolumeFormat for Fat {
    /// Which filesystem this reads.
    ///
    /// One reader serves FAT12, FAT16 and FAT32 because they are one format
    /// with three table widths, and nothing about reading one differs. The
    /// exact answer for a volume needs the volume open - the difference is a
    /// cluster count, not a signature - so it is [`fat_id`], and that refined
    /// answer is what a message names. This names the family.
    fn id(&self) -> FsId {
        FsId::Fat32
    }

    /// What a FAT volume offers.
    ///
    /// Read-only because that is the feature rather than a stage of it. Not
    /// seekable and not randomly accessible because a `fatfs::File` borrows
    /// the `FileSystem` it came from and neither can outlive the call that
    /// opened them, so there is no handle to hand out; the viewer's
    /// forward-only mode is the honest answer and it already exists.
    /// Not executable because a file inside an image has no
    /// path the kernel can be handed.
    ///
    /// No POSIX anywhere: FAT has no mode, no owner, no group, no hard links
    /// and no atomic rename, and this says so instead of reporting the
    /// capabilities of the filesystem the image happens to be stored on.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            writable: false,
            seekable: false,
            random_access: false,
            has_directories: true,
            atomic_rename: false,
            paged_listing: false,
            can_execute: false,
            links: false,
            settable_mode: false,
            latency: LatencyClass::Local,
        }
    }

    /// Walk the whole volume, pushing every member as it is read.
    ///
    /// Depth first, so a panel opened on the root fills from the first
    /// directory read rather than after the last one. Bounded three ways: by
    /// [`MAX_DIR_DEPTH`], by [`MAX_DIR_ENTRIES`] per directory, and by
    /// `index::MAX_MEMBERS` and `index::MAX_NAME_BYTES` on the sink's side
    /// (the design I8).
    ///
    /// Hitting either of this module's two bounds is an error rather than a
    /// silent stop. What has been read is still listed - that is what
    /// `IndexStatus::Failed` means here - but a listing is never quietly
    /// short, and a FAT image nested deeper than 64 or a directory of more
    /// than 65 536 entries is a crafted image rather than an awkward one.
    fn index(&self, region: &Region, sink: &mut dyn IndexSink) -> Result<()> {
        let fs = open(region)?;
        let root = fs.root_dir();
        walk_dir(&root, "", 0, sink)?;
        Ok(())
    }

    /// Copy one member's bytes into `out`.
    ///
    /// A fresh open and a fresh walk, because nothing is held between calls
    /// (the design J14). The caller has already wrapped `out` in
    /// the archive backend's guard, so a member whose directory entry lies
    /// about its size stops at the claim rather than at the end of the disk
    /// (the design I5).
    fn read_member(&self, region: &Region, member: &Member, out: &mut dyn Write) -> Result<u64> {
        if matches!(member.kind, MemberKind::Dir) {
            return Err(Error::InvalidPath(format!(
                "{}: a directory has no contents to read",
                member.path
            )));
        }
        let fs = open(region)?;
        let root = fs.root_dir();
        match walk(&root, &member.path)? {
            Resolved::Dir => Err(Error::InvalidPath(format!(
                "{}: a directory has no contents to read",
                member.path
            ))),
            Resolved::File(mut file) => copy_out(&member.path, &mut file, out),
        }
    }

    /// The volume label, for a status line.
    ///
    /// From the BPB in the boot sector, and deliberately not from the root
    /// directory's volume entry. A FAT volume records the label in both places
    /// and every formatter writes both, so the two agree on every image a tool
    /// produced; the difference is what happens on one that no tool produced.
    /// `fatfs`'s root-directory scan walks a cluster chain to its end, and a
    /// crafted chain that loops has no end, so reading the label there could
    /// hang a status line on a hostile image. The boot sector is 512 bytes at
    /// a fixed offset and cannot.
    ///
    /// `NO NAME` is the placeholder every formatter writes when it was given
    /// no label, so it is reported as no label rather than as a name nobody
    /// chose.
    fn volume_label(&self, region: &Region) -> Option<String> {
        let fs = open(region).ok()?;
        let label = fs
            .volume_label()
            .trim_matches(|c: char| c.is_whitespace() || c == '\u{0}')
            .to_string();
        if label.is_empty() || label.eq_ignore_ascii_case("NO NAME") {
            return None;
        }
        Some(label)
    }
}

/// Which FAT this volume really is (the design, rule 2).
///
/// The superblock cannot tell FAT12 from FAT16: the difference is the number
/// of clusters the BPB's geometry works out to, and `fatfs` computes it when
/// it opens the volume. This is what a message names, so that a floppy image
/// is called FAT12 and not FAT32.
pub fn fat_id(region: &Region) -> Result<FsId> {
    let fs = open(region)?;
    Ok(match fs.fat_type() {
        fatfs::FatType::Fat12 => FsId::Fat12,
        fatfs::FatType::Fat16 => FsId::Fat16,
        fatfs::FatType::Fat32 => FsId::Fat32,
    })
}

/// Open the filesystem on `region`, read-only.
///
/// Opened, used and dropped inside one call, never held: `fatfs::FileSystem`
/// contains a `RefCell` and is therefore not `Sync`, and `Vfs` is
/// `Send + Sync` (the design I11).
///
/// `FsOptions::new()` leaves `update_accessed_date` off, which is its default:
/// with it on, listing a directory would try to write an access date back into
/// every entry it read. The write would be refused twice over, but a reader
/// that has to be refused is a reader that asked.
fn open(region: &Region) -> Result<fatfs::FileSystem<Reader>> {
    let reader = region.open()?;
    fatfs::FileSystem::new(reader, fatfs::FsOptions::new()).map_err(|e| {
        Error::msg(format!(
            "{}: the FAT boot sector is damaged ({e})",
            region.container().display()
        ))
    })
}

/// What a member path resolved to.
///
/// The directory arm carries nothing on purpose: the only question asked of a
/// resolved path here is whether it has bytes, and a directory does not. A
/// handle nobody reads would be a field nobody reads.
enum Resolved<'a> {
    /// A regular file, open and positioned at its first byte.
    File(fatfs::File<'a, Reader>),
    /// A directory, which has no contents to read.
    Dir,
}

/// Resolve a normalised member path to a `fatfs` handle, one component at a
/// time.
///
/// Component by component, never by handing `fatfs::Dir::open_file` a path:
/// that method splits on `/` itself, and the point of walking is that every
/// component reaching it is one `index::Builder` already accepted as a single
/// safe component (the design I2). A path that
/// arrived from anywhere else still cannot smuggle a `..` through, because a
/// component of `.` or `..` is refused here as well.
fn walk<'a>(root: &fatfs::Dir<'a, Reader>, member: &str) -> Result<Resolved<'a>> {
    if member.is_empty() {
        return Ok(Resolved::Dir);
    }
    let mut dir = root.clone();
    let mut rest = member;
    for _ in 0..MAX_DIR_DEPTH {
        let (name, tail) = match rest.split_once('/') {
            Some((name, tail)) => (name, Some(tail)),
            None => (rest, None),
        };
        if !is_plain_name(name) {
            return Err(Error::InvalidPath(format!(
                "{member}: not a name a file in this image can have"
            )));
        }
        let entry = child(&dir, name, member)?;
        match tail {
            Some(tail) if !tail.is_empty() => {
                if !entry.is_dir() {
                    return Err(Error::NotFound(member.to_string()));
                }
                dir = entry.to_dir();
                rest = tail;
            }
            _ => {
                return Ok(if entry.is_dir() {
                    Resolved::Dir
                } else {
                    Resolved::File(entry.to_file())
                });
            }
        }
    }
    Err(too_deep(member))
}

/// The entry named `name` in `dir`, or [`Error::NotFound`] for `whole`.
///
/// Exact, byte for byte, against the name the index holds. FAT itself matches
/// case-insensitively and would answer `README.TXT` for `readme.txt`; the
/// index stores the name as it was decoded, so matching it exactly is what
/// stops two members that differ only in case from resolving to one another.
fn child<'a>(
    dir: &fatfs::Dir<'a, Reader>,
    name: &str,
    whole: &str,
) -> Result<fatfs::DirEntry<'a, Reader>> {
    for (seen, entry) in dir.iter().enumerate() {
        if seen >= MAX_DIR_ENTRIES {
            return Err(endless(whole));
        }
        let entry = entry.map_err(|e| Error::msg(format!("{whole}: {e}")))?;
        let found = entry.file_name();
        if found == "." || found == ".." {
            continue;
        }
        if found == name {
            return Ok(entry);
        }
    }
    Err(Error::NotFound(whole.to_string()))
}

/// Push every member of `dir` and then of everything below it.
///
/// Returns `Ok(false)` once the sink has asked to stop, which every caller
/// above answers by stopping too: the listing must stop promptly
/// when the panel that wanted it has gone.
///
/// Depth first and pushed as it goes, so the panel fills from the first
/// directory read. Every decoded name is checked with
/// [`crate::vfs::is_plain_name`] before it becomes part of a member path, and
/// the test is on the decoded name because both the long-name and the
/// short-name path end there (the design I8). That subsumes `.`
/// and `..`, which `fatfs` yields in every subdirectory, and it is the same
/// guard the ISO reader applies to a Rock Ridge name.
fn walk_dir(
    dir: &fatfs::Dir<'_, Reader>,
    prefix: &str,
    depth: usize,
    sink: &mut dyn IndexSink,
) -> Result<bool> {
    if depth >= MAX_DIR_DEPTH {
        return Err(too_deep(if prefix.is_empty() { "/" } else { prefix }));
    }
    for (seen, entry) in dir.iter().enumerate() {
        if sink.cancelled() {
            return Ok(false);
        }
        if seen >= MAX_DIR_ENTRIES {
            return Err(endless(if prefix.is_empty() { "/" } else { prefix }));
        }
        let entry = entry.map_err(|e| {
            Error::msg(format!(
                "{}: the directory could not be read ({e})",
                if prefix.is_empty() { "/" } else { prefix }
            ))
        })?;
        // A long file name is 13 UTF-16 code units per directory entry and
        // `fatfs` concatenates them without asking what is in them, so a
        // crafted image can name a file `/etc/cron.d/pwn` or `../..`. The name
        // is folded into a member path one line down and joined onto a
        // destination by every copy that follows, so nothing but one ordinary
        // component may reach that: `Path::join` with an absolute argument
        // discards the base entirely. A row that is not one is dropped and the
        // rest of the image is still listed, exactly as the ISO reader drops
        // one.
        let Some(name) = PlainName::new(entry.file_name()) else {
            continue;
        };
        let path = if prefix.is_empty() {
            name.into_string()
        } else {
            format!("{prefix}/{name}")
        };
        let is_dir = entry.is_dir();
        let raw = RawMember {
            name: path.clone(),
            kind: if is_dir {
                MemberKind::Dir
            } else {
                MemberKind::File
            },
            // A directory's entry records no size, and the panel renders
            // `<DIR>` there in any case.
            size: if is_dir { 0 } else { entry.len() },
            mtime: to_system_time(entry.modified()),
            // FAT has nowhere to store any of these three. Left at zero
            // rather than invented; see this module's header.
            mode: 0,
            uid: 0,
            gid: 0,
            // A FAT member is not a byte range of the container: its bytes are
            // a cluster chain, so there is no offset to record and it is found
            // again by walking to its path. Nothing else in this crate reads a
            // locator to decide whether a member can be read.
            locator: Locator::None,
        };
        if !sink.push(raw) {
            return Ok(false);
        }
        if is_dir && !walk_dir(&entry.to_dir(), &path, depth.saturating_add(1), sink)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Copy a member's bytes into `out`, counting them.
///
/// A fixed buffer and a loop rather than `std::io::copy`, so that the failure
/// carries the member's name: the caller's guard reports a member that lied
/// about its size through this write, and "the image" would not say which file
/// it meant.
fn copy_out(path: &str, file: &mut fatfs::File<'_, Reader>, out: &mut dyn Write) -> Result<u64> {
    let mut buf = vec![0u8; COPY_BUFFER];
    let mut written = 0u64;
    loop {
        let read = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Error::msg(format!("{path}: {e}"))),
        };
        let Some(chunk) = buf.get(..read) else {
            break;
        };
        out.write_all(chunk)
            .map_err(|e| Error::msg(format!("{path}: {e}")))?;
        written = written.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
    }
    Ok(written)
}

/// A DOS timestamp as a [`SystemTime`], or `None` when it is not a time.
///
/// A DOS date and time are packed into two 16-bit words with no validation
/// anywhere in the format, so a directory entry can say month 0, day 0 or the
/// sixty-third second. Every one of those answers `None`: a wrong modification
/// time is not worth failing a listing over, and an empty date column says
/// less that is false than a made-up one
/// (the design, rule 4).
///
/// The conversion is written out rather than taken from `fatfs`'s own
/// `From<DateTime> for chrono::DateTime<Local>`, which builds the date with
/// `Local.ymd` and panics on exactly the values a crafted image supplies.
fn to_system_time(dt: fatfs::DateTime) -> Option<SystemTime> {
    let date = NaiveDate::from_ymd_opt(
        i32::from(dt.date.year),
        u32::from(dt.date.month),
        u32::from(dt.date.day),
    )?;
    let time = NaiveTime::from_hms_milli_opt(
        u32::from(dt.time.hour),
        u32::from(dt.time.min),
        u32::from(dt.time.sec),
        u32::from(dt.time.millis),
    )?;
    // A DOS timestamp is local time with no zone in it, which is why the
    // ambiguous hour of a daylight-saving change has two answers and the
    // skipped hour has none. `single` takes neither rather than picking one.
    let local = chrono::Local
        .from_local_datetime(&date.and_time(time))
        .single()?;
    let secs = u64::try_from(local.timestamp()).ok()?;
    Some(UNIX_EPOCH + Duration::new(secs, local.timestamp_subsec_nanos()))
}

/// The refusal for a directory tree deeper than this reader walks.
fn too_deep(what: &str) -> Error {
    Error::msg(format!(
        "{what}: this image nests directories deeper than {MAX_DIR_DEPTH}, \
         which a directory that contains itself also does"
    ))
}

/// The refusal for a directory whose entries do not end.
fn endless(what: &str) -> Error {
    Error::msg(format!(
        "{what}: this directory holds more than {MAX_DIR_ENTRIES} entries, \
         which a directory whose clusters form a loop also does"
    ))
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Seek as _, SeekFrom};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use fatfs::{FatType, FormatVolumeOptions};

    use super::*;
    use crate::vfs::archive::index::{Builder, Index, IndexStatus};

    /// A directory of this process's own, so two suites running at once do not
    /// share a fixture.
    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hcmd-fat-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// A formatted FAT volume, built by `fatfs` itself.
    ///
    /// The fixture is written by the library that reads it, which is the only
    /// way to be sure the test is about this module rather than about a
    /// hand-rolled boot sector: every byte of geometry, FAT and directory
    /// entry is the crate's own.
    fn volume(bytes: usize, options: FormatVolumeOptions, files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut disk = Cursor::new(vec![0u8; bytes]);
        fatfs::format_volume(&mut disk, options).expect("format");
        disk.seek(SeekFrom::Start(0)).expect("rewind");
        {
            let fs = fatfs::FileSystem::new(&mut disk, fatfs::FsOptions::new()).expect("open");
            let root = fs.root_dir();
            for (path, bytes) in files {
                let mut walked = String::new();
                let components: Vec<&str> = path.split('/').collect();
                for dir in components.iter().take(components.len().saturating_sub(1)) {
                    if !walked.is_empty() {
                        walked.push('/');
                    }
                    walked.push_str(dir);
                    let _ = root.create_dir(&walked);
                }
                if path.ends_with('/') {
                    continue;
                }
                let mut file = root.create_file(path).expect("create file");
                file.write_all(bytes).expect("write");
                file.flush().expect("flush");
            }
        }
        disk.into_inner()
    }

    /// The default fixture: a small FAT12 volume with two files and a nested
    /// directory.
    fn small(tag: &str) -> (PathBuf, Region) {
        let image = volume(
            1024 * 1024,
            FormatVolumeOptions::new().volume_label(*b"HCMD TEST  "),
            &[
                ("top.txt", b"top"),
                ("d/inner.bin", &[7u8; 5000]),
                ("d/e/deep.txt", b"deep"),
            ],
        );
        at(tag, &image, 0)
    }

    /// Write `image` into a container, `pad` zero bytes before it, and return
    /// the region that addresses it.
    ///
    /// `pad` is how a partition is simulated without a partition table: the
    /// volume does not begin at byte 0 of the file, so a reader that forgets
    /// its region's start reads the padding and fails.
    fn at(tag: &str, image: &[u8], pad: usize) -> (PathBuf, Region) {
        let dir = temp(tag);
        let path = dir.join("image.img");
        let mut file = std::fs::File::create(&path).expect("create container");
        file.write_all(&vec![0u8; pad]).expect("pad");
        file.write_all(image).expect("body");
        file.write_all(&[0u8; 4096]).expect("trailer");
        file.flush().expect("flush");
        drop(file);
        let region = Region::sub(&path, pad as u64, image.len() as u64).expect("region");
        (path, region)
    }

    /// Index `region` with the real sink, exactly as the backend does.
    fn index_of(region: &Region) -> (Arc<Index>, Result<()>) {
        let index = Arc::new(Index::new());
        let outcome = {
            let mut sink = Builder::new(Arc::clone(&index), false);
            Fat.index(region, &mut sink)
        };
        let status = match &outcome {
            Ok(()) => IndexStatus::Complete,
            Err(err) => IndexStatus::Failed(err.to_string()),
        };
        index.finish(status);
        (index, outcome)
    }

    fn names(index: &Index, dir: &str) -> Vec<String> {
        let (_, rows, _) = index.children_from(dir, 0);
        rows.into_iter().map(|row| row.name).collect()
    }

    fn clean(path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn a_formatted_volume_lists_its_members() {
        let (path, region) = small("list");
        let (index, outcome) = index_of(&region);
        outcome.expect("index");
        assert_eq!(index.status(), IndexStatus::Complete);

        let mut top = names(&index, "");
        top.sort();
        assert_eq!(top, vec!["d".to_string(), "top.txt".to_string()]);

        let mut inner = names(&index, "d");
        inner.sort();
        assert_eq!(inner, vec!["e".to_string(), "inner.bin".to_string()]);
        assert_eq!(names(&index, "d/e"), vec!["deep.txt".to_string()]);

        assert!(index.is_dir("d"));
        assert!(index.is_dir("d/e"));
        let member = index.get("d/inner.bin").expect("inner");
        assert_eq!(member.size, 5000);
        clean(&path);
    }

    #[test]
    fn dot_and_dot_dot_are_never_listed() {
        // `fatfs` yields `.` and `..` in every subdirectory, so each listing's
        // length comes first: a walk that produced nothing would run the loop
        // below zero times and look like one that filtered them out.
        let (path, region) = small("dots");
        let (index, _) = index_of(&region);
        for (dir, count) in [("", 2), ("d", 2), ("d/e", 1)] {
            let listed = names(&index, dir);
            assert_eq!(listed.len(), count, "{dir:?} listed {listed:?}");
            for name in listed {
                assert!(name != "." && name != "..", "{dir} listed {name}");
            }
        }
        assert!(index.get(".").is_none());
        assert!(index.get("d/..").is_none());
        clean(&path);
    }

    /// A long file name with a `/` in it, which no formatter writes and
    /// nothing but a crafted image has.
    ///
    /// `fatfs` builds a long name by concatenating the UTF-16 code units of
    /// its directory entries and never asks what is in them, so the name it
    /// hands back is whatever the image says. The fixture is `fatfs`'s own
    /// output with one code unit overwritten: the `-` of `evil-name.txt`
    /// becomes `/`, which leaves the short-name checksum the long entry
    /// carries untouched, so the entry is still a valid one that `fatfs`
    /// reads.
    #[test]
    fn a_crafted_long_name_that_is_a_path_is_not_listed_at_all() {
        let mut image = volume(
            1024 * 1024,
            FormatVolumeOptions::new(),
            &[("good.txt", b"good"), ("evil-name.txt", b"evil")],
        );
        // The first five characters of the long name, UTF-16LE, at offset 1 of
        // the long-name entry. The short name is `EVIL-N~1TXT`, in ASCII and
        // uppercase, so this pattern is the long entry and nothing else.
        let needle: Vec<u8> = "evil-".encode_utf16().flat_map(u16::to_le_bytes).collect();
        let entry = image
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("the long name entry fatfs wrote");
        let dash = entry.saturating_add(needle.len()).saturating_sub(2);
        let slot = image.get_mut(dash).expect("the code unit to overwrite");
        *slot = b'/';

        let (path, region) = at("crafted", &image, 0);
        let (index, outcome) = index_of(&region);
        outcome.expect("the rest of the image still lists");

        assert_eq!(
            names(&index, ""),
            vec!["good.txt".to_string()],
            "the crafted row is dropped and the ordinary one is kept"
        );
        assert!(index.get("evil/name.txt").is_none());
        assert!(index.get("evil").is_none(), "and no directory was invented");
        assert!(names(&index, "evil").is_empty());
        clean(&path);
    }

    #[test]
    fn fat_has_no_posix_metadata_and_says_so() {
        let (path, region) = small("posix");
        let (index, _) = index_of(&region);
        for name in ["top.txt", "d", "d/inner.bin", "d/e/deep.txt"] {
            let member = index.get(name).expect(name);
            assert_eq!(member.mode, 0, "{name} invented a mode");
            assert_eq!(member.uid, 0, "{name} invented an owner");
            assert_eq!(member.gid, 0, "{name} invented a group");
            assert!(
                !matches!(member.kind, MemberKind::Symlink(_)),
                "{name} claimed to be a symbolic link"
            );
        }

        let caps = Fat.capabilities();
        assert!(!caps.writable, "a disk image is read-only");
        assert!(!caps.seekable);
        assert!(!caps.random_access);
        assert!(!caps.can_execute);
        assert!(!caps.atomic_rename);
        assert!(caps.has_directories);
        clean(&path);
    }

    #[test]
    fn a_member_reads_back_the_bytes_that_were_written() {
        let (path, region) = small("read");
        let (index, _) = index_of(&region);

        let mut got = Vec::new();
        let member = index.get("top.txt").expect("top");
        let written = Fat.read_member(&region, &member, &mut got).expect("read");
        assert_eq!(got, b"top");
        assert_eq!(written, 3);

        let mut got = Vec::new();
        let member = index.get("d/inner.bin").expect("inner");
        let written = Fat.read_member(&region, &member, &mut got).expect("read");
        assert_eq!(written, 5000);
        assert_eq!(got, vec![7u8; 5000]);

        let mut got = Vec::new();
        let member = index.get("d/e/deep.txt").expect("deep");
        Fat.read_member(&region, &member, &mut got).expect("read");
        assert_eq!(got, b"deep");
        clean(&path);
    }

    #[test]
    fn a_member_larger_than_one_copy_buffer_reads_whole() {
        let body: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let image = volume(
            4 * 1024 * 1024,
            FormatVolumeOptions::new(),
            &[("big.bin", &body)],
        );
        let (path, region) = at("big", &image, 0);
        let (index, _) = index_of(&region);
        let member = index.get("big.bin").expect("big");
        assert_eq!(member.size, body.len() as u64);
        let mut got = Vec::new();
        let written = Fat.read_member(&region, &member, &mut got).expect("read");
        assert_eq!(written, body.len() as u64);
        assert_eq!(got, body);
        clean(&path);
    }

    #[test]
    fn a_volume_inside_a_container_reads_only_its_own_bytes() {
        let image = volume(
            1024 * 1024,
            FormatVolumeOptions::new(),
            &[("only.txt", b"partition two")],
        );
        // Padded, so byte 0 of the region is not byte 0 of the file: a reader
        // that lost its region's start reads zeros and fails to open.
        let (path, region) = at("region", &image, 1024 * 1024);
        assert_eq!(region.start(), 1024 * 1024);
        let (index, outcome) = index_of(&region);
        outcome.expect("index");
        assert_eq!(names(&index, ""), vec!["only.txt".to_string()]);

        let member = index.get("only.txt").expect("only");
        let mut got = Vec::new();
        Fat.read_member(&region, &member, &mut got).expect("read");
        assert_eq!(got, b"partition two");

        // The padding in front of it is not a FAT volume, and asking for it
        // fails rather than reading the volume behind it.
        let padding = Region::sub(&path, 0, 1024 * 1024).expect("padding region");
        assert!(
            Fat.index(&padding, &mut Builder::new(Arc::new(Index::new()), false))
                .is_err()
        );
        clean(&path);
    }

    #[test]
    fn the_three_fats_are_told_apart_by_opening_them() {
        let twelve = volume(1024 * 1024, FormatVolumeOptions::new(), &[]);
        let (path, region) = at("fat12", &twelve, 0);
        assert_eq!(fat_id(&region).expect("fat12"), FsId::Fat12);
        clean(&path);

        // The smallest volume each table width is legal on, near enough:
        // FAT16 needs 4 085 clusters and FAT32 needs 65 525, and a fixture
        // larger than that is temporary-directory space spent on nothing.
        let sixteen = volume(
            4 * 1024 * 1024,
            FormatVolumeOptions::new()
                .fat_type(FatType::Fat16)
                .bytes_per_cluster(512),
            &[],
        );
        let (path, region) = at("fat16", &sixteen, 0);
        assert_eq!(fat_id(&region).expect("fat16"), FsId::Fat16);
        clean(&path);

        let thirty_two = volume(
            36 * 1024 * 1024,
            FormatVolumeOptions::new()
                .fat_type(FatType::Fat32)
                .bytes_per_cluster(512),
            &[],
        );
        let (path, region) = at("fat32", &thirty_two, 0);
        assert_eq!(fat_id(&region).expect("fat32"), FsId::Fat32);
        clean(&path);
    }

    #[test]
    fn a_directory_has_no_contents_to_read() {
        let (path, region) = small("dir-read");
        let (index, _) = index_of(&region);
        let member = index.get("d").expect("d");
        let mut got = Vec::new();
        let err = Fat
            .read_member(&region, &member, &mut got)
            .expect_err("a directory is not readable");
        assert!(err.to_string().contains("directory"), "{err}");
        assert!(got.is_empty());
        clean(&path);
    }

    #[test]
    fn a_member_path_that_escapes_is_refused_by_the_walk() {
        let (path, region) = small("escape");
        // The index can never hold such a name - `normalize_member` refuses it
        // before it is stored - so this is the second of the two checks, on
        // the path a caller supplies directly.
        for escape in ["../top.txt", "d/../../top.txt", "./top.txt", "d//top.txt"] {
            let member = Member {
                path: escape.to_string(),
                kind: MemberKind::File,
                size: 3,
                mtime: None,
                mode: 0,
                uid: 0,
                gid: 0,
                locator: Locator::None,
                synthetic: false,
            };
            let mut got = Vec::new();
            assert!(
                Fat.read_member(&region, &member, &mut got).is_err(),
                "{escape} was not refused"
            );
            assert!(got.is_empty(), "{escape} produced bytes");
        }
        clean(&path);
    }

    #[test]
    fn a_member_that_is_not_there_is_not_found() {
        let (path, region) = small("missing");
        let member = Member {
            path: "nowhere.txt".to_string(),
            kind: MemberKind::File,
            size: 0,
            mtime: None,
            mode: 0,
            uid: 0,
            gid: 0,
            locator: Locator::None,
            synthetic: false,
        };
        let mut got = Vec::new();
        let err = Fat
            .read_member(&region, &member, &mut got)
            .expect_err("not found");
        assert!(matches!(err, Error::NotFound(_)), "{err}");
        clean(&path);
    }

    #[test]
    fn a_volume_label_is_read_and_a_placeholder_is_not() {
        let (path, region) = small("label");
        assert_eq!(Fat.volume_label(&region).as_deref(), Some("HCMD TEST"));
        clean(&path);

        let unlabelled = volume(1024 * 1024, FormatVolumeOptions::new(), &[]);
        let (path, region) = at("no-label", &unlabelled, 0);
        assert_eq!(
            Fat.volume_label(&region),
            None,
            "NO NAME is the absence of a label, not a label"
        );
        clean(&path);
    }

    #[test]
    fn a_damaged_boot_sector_is_named_as_damaged() {
        let (path, region) = at("damaged", &vec![0u8; 1024 * 1024], 0);
        let err = Fat
            .index(&region, &mut Builder::new(Arc::new(Index::new()), false))
            .expect_err("zeros are not a FAT volume");
        let message = err.to_string();
        assert!(message.contains("boot sector"), "{message}");
        assert!(
            !message.contains("not supported"),
            "damaged and unsupported are different problems: {message}"
        );
        clean(&path);
    }

    #[test]
    fn a_member_is_not_seekable_and_says_so() {
        let (path, region) = small("seek");
        let (index, _) = index_of(&region);
        let member = index.get("top.txt").expect("top");
        assert!(
            Fat.open_member(&region, &member).expect("open").is_none(),
            "a fatfs::File cannot outlive the FileSystem it borrows"
        );
        clean(&path);
    }

    #[test]
    fn an_index_cancelled_before_it_starts_says_nothing_at_all() {
        let (path, region) = small("cancel");
        let index = Arc::new(Index::new());
        index.cancel();
        let mut sink = Builder::new(Arc::clone(&index), false);
        Fat.index(&region, &mut sink)
            .expect("a cancelled walk ends");
        drop(sink);
        assert_eq!(index.len(), 0, "nothing is pushed after the cancel");
        clean(&path);
    }

    /// A sink that cancels the walk the moment it has taken `after` members,
    /// so the cancel arrives from *inside* the directory loop rather than
    /// before it.
    struct CancelAfter {
        /// The real sink, so what is pushed lands in a real index.
        inner: Builder,
        /// How many members have been pushed.
        seen: usize,
        /// How many it takes before it asks the walk to stop.
        after: usize,
    }

    impl IndexSink for CancelAfter {
        fn push(&mut self, raw: RawMember) -> bool {
            self.seen = self.seen.saturating_add(1);
            self.inner.push(raw)
        }

        fn cancelled(&self) -> bool {
            self.seen >= self.after || self.inner.cancelled()
        }
    }

    /// The half that matters when a panel is closed over a big image: the
    /// cancel arrives while the walk is running.
    ///
    /// A walk that reads the flag once on the way in and never again passes
    /// the test above and keeps reading a whole volume nobody is waiting for,
    /// so this one lets a member through first and then asserts the walk
    /// stopped where it was told to rather than at the end of the volume.
    #[test]
    fn a_cancel_part_way_through_stops_the_walk_where_it_was() {
        const MEMBERS: usize = 40;
        let owned: Vec<String> = (0..MEMBERS).map(|n| format!("f{n:02}.txt")).collect();
        let files: Vec<(&str, &[u8])> = owned
            .iter()
            .map(|name| (name.as_str(), b"x".as_slice()))
            .collect();
        let image = volume(1024 * 1024, FormatVolumeOptions::new(), &files);
        let (path, region) = at("midcancel", &image, 0);

        // The whole volume first, so "it stopped early" is measured against a
        // count this fixture really has.
        let (whole, outcome) = index_of(&region);
        outcome.expect("index");
        assert_eq!(whole.len(), MEMBERS, "the fixture holds every member");

        let index = Arc::new(Index::new());
        {
            let mut sink = CancelAfter {
                inner: Builder::new(Arc::clone(&index), false),
                seen: 0,
                after: 1,
            };
            Fat.index(&region, &mut sink)
                .expect("a cancelled walk ends");
        }
        assert_eq!(
            index.len(),
            1,
            "the walk read on past the cancel: {MEMBERS} members in this volume"
        );
        clean(&path);
    }

    #[test]
    fn a_dos_timestamp_that_is_not_a_time_is_no_time_at_all() {
        let epoch = fatfs::DateTime {
            date: fatfs::Date {
                year: 1980,
                month: 1,
                day: 1,
            },
            time: fatfs::Time {
                hour: 0,
                min: 0,
                sec: 0,
                millis: 0,
            },
        };
        assert!(to_system_time(epoch).is_some(), "1980-01-01 is a date");

        // Every one of these is a bit pattern a directory entry can hold and
        // no calendar can.
        let bad = [
            (0u16, 0u16, 0u16, 0u16, 0u16, 0u16),
            (1980, 13, 1, 0, 0, 0),
            (1980, 2, 30, 0, 0, 0),
            (1980, 1, 1, 24, 0, 0),
            (1980, 1, 1, 0, 60, 0),
            (1980, 1, 1, 0, 0, 63),
        ];
        for (year, month, day, hour, min, sec) in bad {
            let dt = fatfs::DateTime {
                date: fatfs::Date { year, month, day },
                time: fatfs::Time {
                    hour,
                    min,
                    sec,
                    millis: 0,
                },
            };
            assert!(
                to_system_time(dt).is_none(),
                "{year}-{month}-{day} {hour}:{min}:{sec} was accepted"
            );
        }
    }

    #[test]
    fn a_real_file_carries_the_time_the_fixture_gave_it() {
        let (path, region) = small("mtime");
        let (index, _) = index_of(&region);
        let member = index.get("top.txt").expect("top");
        assert!(
            member.mtime.is_some(),
            "a FAT directory entry records a modification time"
        );
        clean(&path);
    }

    /// The offset of the 32-byte directory entry whose short name is `name`
    /// and whose attribute byte marks it a directory.
    ///
    /// Every directory region of a FAT volume starts at a multiple of the
    /// sector size, so a directory entry is always 32-byte aligned from the
    /// start of the image and a strided scan finds it.
    fn dir_entry_at(image: &[u8], name: &[u8; 11]) -> usize {
        (0..image.len().saturating_sub(32))
            .step_by(32)
            .find(|&at| {
                image.get(at..at.saturating_add(11)) == Some(&name[..])
                    && image
                        .get(at.saturating_add(11))
                        .is_some_and(|a| a & 0x10 != 0)
            })
            .expect("a directory entry with that short name")
    }

    /// The starting cluster a directory entry names, high word and low.
    fn first_cluster(image: &[u8], at: usize) -> u32 {
        let lo = u32::from(u16::from_le_bytes([image[at + 26], image[at + 27]]));
        let hi = u32::from(u16::from_le_bytes([image[at + 20], image[at + 21]]));
        (hi << 16) | lo
    }

    /// Point a directory entry at `cluster`, which nothing in the format stops
    /// from being an ancestor's.
    fn set_first_cluster(image: &mut [u8], at: usize, cluster: u32) {
        let lo = u16::try_from(cluster & 0xFFFF).expect("low word");
        let hi = u16::try_from(cluster >> 16).expect("high word");
        image[at + 26..at + 28].copy_from_slice(&lo.to_le_bytes());
        image[at + 20..at + 22].copy_from_slice(&hi.to_le_bytes());
    }

    #[test]
    fn a_directory_that_contains_itself_ends_the_walk_instead_of_hanging() {
        let mut image = volume(
            1024 * 1024,
            FormatVolumeOptions::new(),
            &[("a/b/keep.txt", b"keep")],
        );
        // `a/b` is made to start at `a`'s own cluster, so walking into it
        // finds `b` again, for ever. `fatfs` does not check this and neither
        // does the format: a starting cluster is a number in a 32-byte record
        // and nothing says it may not be an ancestor's.
        let a = dir_entry_at(&image, b"A          ");
        let b = dir_entry_at(&image, b"B          ");
        let cluster = first_cluster(&image, a);
        assert!(
            cluster >= 2,
            "a real directory starts at cluster 2 or later"
        );
        set_first_cluster(&mut image, b, cluster);

        let (path, region) = at("cycle", &image, 0);
        let index = Arc::new(Index::new());
        let outcome = {
            let mut sink = Builder::new(Arc::clone(&index), false);
            Fat.index(&region, &mut sink)
        };
        let err = outcome.expect_err("a cycle is refused rather than followed");
        assert!(err.to_string().contains("deeper than"), "{err}");
        // Bounded, not merely finite: the walk stopped at the depth limit, so
        // the index holds one member per level and not a million.
        assert!(
            index.len() <= MAX_DIR_DEPTH.saturating_add(4),
            "the walk went {} members deep",
            index.len()
        );
        // And what it did read is still there to list, which is what a failed
        // index means here.
        assert!(index.is_dir("a"));
        assert!(index.is_dir("a/b"));
        clean(&path);
    }

    #[test]
    fn nothing_in_this_module_can_write_to_the_container() {
        let (path, region) = small("readonly");
        let before = std::fs::read(&path).expect("before");
        let (index, _) = index_of(&region);
        let member = index.get("top.txt").expect("top");
        let mut got = Vec::new();
        Fat.read_member(&region, &member, &mut got).expect("read");
        let _ = Fat.volume_label(&region);
        let after = std::fs::read(&path).expect("after");
        assert_eq!(before, after, "reading an image changed it");

        // The handle itself refuses, before the kernel gets the chance to.
        let mut reader = region.open().expect("open");
        let err = std::io::Write::write(&mut reader, b"x").expect_err("refused");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        clean(&path);
    }
}
