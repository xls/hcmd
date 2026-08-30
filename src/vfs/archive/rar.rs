//! `.rar` - **read only**.
//!
//! > `.rar` | yes | no | `unrar` (crate, bundled bindings - RAR is
//! > patent-encumbered for writing)
//!
//! So [`RarFormat::write_model`] is [`WriteModel::None`], `Capabilities`
//! reports `writable: false`, and `F5` *into* a `.rar` is refused up front
//! with a clear message rather than failing halfway through a copy.
//! That refusal is the whole reason `Capabilities` exists.
//!
//! # Why reads are materialised
//!
//! `unrar` offers exactly two ways to get a member out: into a `Vec<u8>`, or
//! into a file. The first is the whole member in memory, which a 4 GB member
//! makes unacceptable, so this format declares
//! [`MemberSource::Materialise`] and [`super::ArchiveFs`] extracts through the
//! session cache - bounded by disk, cleaned up on exit, and shared between
//! panels. It is the same mechanism nested archives use, which
//! is why there is one of it rather than two.
//!
//! # Path separators
//!
//! RAR stores `\` as its separator, so this is the one format for which
//! [`super::safety::normalize_member`] treats a backslash as a separator
//! rather than as an ordinary Unix filename character.

use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};

use super::format::{ArchiveFormat, FormatId, MemberSource, WriteModel};
use super::index::{IndexSink, Locator, Member, MemberKind, RawMember};

/// The rar backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RarFormat;

/// A path the C library can be handed at all.
///
/// `unrar`'s open functions are documented to **panic** on a filename
/// containing a NUL. A `Path` from the kernel never has one, so this can only
/// fire on a path this program invented - and turning a panic into an error is
/// worth four lines whatever the odds (nothing panics on data).
fn checked(container: &Path) -> Result<&Path> {
    if container.as_os_str().as_bytes().contains(&0) {
        return Err(Error::InvalidPath(format!(
            "{}: a NUL byte in the path",
            container.display()
        )));
    }
    Ok(container)
}

/// A DOS timestamp: seconds/2, minutes, hours, day, month, year since 1980.
///
/// Interpreted as local time, which is what DOS timestamps are, and `None`
/// rather than a guess when the fields do not describe a real instant.
fn dos_time(raw: u32) -> Option<SystemTime> {
    use chrono::TimeZone as _;
    let second = (raw & 0b1_1111).saturating_mul(2);
    let minute = (raw >> 5) & 0b11_1111;
    let hour = (raw >> 11) & 0b1_1111;
    let day = (raw >> 16) & 0b1_1111;
    let month = (raw >> 21) & 0b1111;
    let year = ((raw >> 25) & 0b111_1111).saturating_add(1980);
    let date = chrono::NaiveDate::from_ymd_opt(i32::try_from(year).ok()?, month, day)?;
    let naive = date.and_hms_opt(hour, minute, second)?;
    let local = chrono::Local.from_local_datetime(&naive).single()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(u64::try_from(local.timestamp()).ok()?))
}

/// Phrase an `unrar` failure in terms of the file it happened on.
fn rar_error(container: &Path, err: &unrar::error::UnrarError) -> Error {
    Error::msg(format!("{}: {err}", container.display()))
}

/// Does this header's attribute word describe a Unix symbolic link?
///
/// **This is a security check, not a display nicety**.
/// `libunrar` reproduces such a member by calling `symlink(2)` at the path it
/// is given, and its own escape test counts the `..` in the target against the
/// depth of *that* path rather than against the archive's root - so a crafted
/// `.rar` whose member is `a/b/c/d/e/f/link -> ../../../secret` passes it and
/// plants a link to an arbitrary readable file in this session's cache. Every
/// later read of that "member" then reads the file the link names: an archive
/// that contains nothing at all produces the contents of `~/.ssh/id_rsa`, and
/// the job reports complete success.
///
/// Indexing it as something that is not a file is the first of the two
/// answers. The second is [`super::ArchiveFs::extract_guarded`], which refuses
/// anything that is not a regular file whatever the header said, because this
/// one cannot be conclusive: `unrar` reports the host's raw attribute word
/// without saying which host wrote it, so a DOS attribute word is being read
/// with Unix eyes. `0xA000` is `S_IFLNK`, and the same two bits in a Windows
/// attribute word are `INTEGRITY_STREAM | NOT_CONTENT_INDEXED` - a combination
/// that also requires every other Windows attribute, `ARCHIVE` included, to be
/// clear. The cost of that coincidence is one entry listed as neither a file
/// nor a directory; the cost of not checking is the paragraph above.
fn is_unix_symlink(attr: u32) -> bool {
    /// `S_IFMT`.
    const FORMAT: u32 = 0xF000;
    /// `S_IFLNK`.
    const LINK: u32 = 0xA000;
    // A Unix mode fits in sixteen bits. Anything above them is a word this
    // rule has no business reading.
    attr >> 16 == 0 && attr & FORMAT == LINK
}

impl ArchiveFormat for RarFormat {
    fn id(&self) -> FormatId {
        FormatId::Rar
    }

    fn write_model(&self) -> WriteModel {
        WriteModel::None
    }

    fn member_source(&self) -> MemberSource {
        MemberSource::Materialise
    }

    fn backslash_separators(&self) -> bool {
        true
    }

    fn index(&self, container: &Path, sink: &mut dyn IndexSink) -> Result<()> {
        let container = checked(container)?;
        let archive = unrar::Archive::new(container)
            .open_for_listing()
            .map_err(|e| rar_error(container, &e))?;

        for (ordinal, item) in archive.enumerate() {
            if sink.cancelled() {
                return Ok(());
            }
            // A header that will not parse stops the listing; what was read
            // stays listed.
            let header = item.map_err(|e| rar_error(container, &e))?;
            let raw = RawMember {
                name: header.filename.to_string_lossy().into_owned(),
                kind: if header.is_directory() {
                    MemberKind::Dir
                } else if is_unix_symlink(header.file_attr) {
                    // Not a file, and deliberately not a `Symlink` either:
                    // the target is not in anything `unrar`'s listing API
                    // hands over, and the only way the library will produce
                    // one is by creating a real link at a path we choose. See
                    // `is_unix_symlink` for why that is refused rather than
                    // driven.
                    MemberKind::Other
                } else {
                    MemberKind::File
                },
                size: header.unpacked_size,
                mtime: dos_time(header.file_time),
                // `unrar` reports the host's raw attribute word without saying
                // which host wrote it, so there is no way to tell a Unix mode
                // from a DOS attribute byte. `0` is what the design asks for
                // when a backend has no concept of mode bits.
                mode: 0,
                uid: 0,
                gid: 0,
                locator: Locator::Ordinal(ordinal),
            };
            if !sink.push(raw) {
                return Ok(());
            }
        }
        Ok(())
    }

    fn extract_member(&self, container: &Path, member: &Member, dest: &Path) -> Result<u64> {
        let container = checked(container)?;
        let dest = checked(dest)?;
        let Locator::Ordinal(wanted) = member.locator else {
            return Err(Error::InvalidPath(format!(
                "{}: a directory has no contents to read",
                member.path
            )));
        };

        let mut cursor = unrar::Archive::new(container)
            .open_for_processing()
            .map_err(|e| rar_error(container, &e))?;
        let mut ordinal = 0usize;
        loop {
            let Some(at_file) = cursor.read_header().map_err(|e| rar_error(container, &e))? else {
                return Err(Error::NotFound(format!(
                    "{} in {}",
                    member.path,
                    container.display()
                )));
            };
            if ordinal == wanted {
                let entry = at_file.entry();
                let size = entry.unpacked_size;
                if is_unix_symlink(entry.file_attr) {
                    // Refused before the library is asked, so it never gets to
                    // call `symlink(2)` on a path this program chose. See
                    // `is_unix_symlink`.
                    return Err(Error::InvalidPath(format!(
                        "{}: refused - a symbolic link in a rar archive \
",
                        member.path
                    )));
                }
                at_file
                    .extract_to(dest)
                    .map_err(|e| rar_error(container, &e))?;
                return Ok(size);
            }
            cursor = at_file.skip().map_err(|e| rar_error(container, &e))?;
            ordinal = ordinal.saturating_add(1);
        }
    }
}

/// Hand-built RAR containers, for the tests in this module and in
/// [`super::tests`].
///
/// the crate table cannot *write* a RAR - that is what "read only"
/// means - and the design forbids shelling out to `rar` to make a fixture, so
/// a container is assembled here from the format itself. That is not a RAR
/// *writer*: it emits stored members with no compression, which is enough to
/// prove that listing, metadata and extraction work against the real `unrar`,
/// and it lives in `#[cfg(test)]`.
#[cfg(test)]
pub(super) mod fixture {
    use std::path::Path;

    /// The attribute word an ordinary Unix file header carries.
    pub(crate) const FILE_ATTR: u32 = 0o644;

    /// `S_IFLNK | 0777`: the attribute word `libunrar` reproduces by calling
    /// `symlink(2)` at the path it is handed.
    pub(crate) const SYMLINK_ATTR: u32 = 0xA1FF;

    /// CRC-32, which a RAR block header carries the low sixteen bits of.
    ///
    /// Written out rather than pulled in: `crc32fast` is not one of the
    /// crates, and this is eleven lines used by the fixtures below and by
    /// nothing that ships.
    pub(crate) fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for byte in data {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = 0u32.wrapping_sub(crc & 1);
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
        !crc
    }

    /// One RAR 4.x block: `HEAD_CRC`, `HEAD_TYPE`, `HEAD_FLAGS`, `HEAD_SIZE`,
    /// the type's own fields, and whatever data follows the header.
    pub(crate) fn rar_block(kind: u8, flags: u16, body: &[u8], data: &[u8]) -> Vec<u8> {
        let size = u16::try_from(7usize.saturating_add(body.len())).expect("a short header");
        let mut head = vec![kind];
        head.extend_from_slice(&flags.to_le_bytes());
        head.extend_from_slice(&size.to_le_bytes());
        head.extend_from_slice(body);
        let mut out = u16::try_from(crc32(&head) & 0xffff)
            .expect("sixteen bits")
            .to_le_bytes()
            .to_vec();
        out.extend_from_slice(&head);
        out.extend_from_slice(data);
        out
    }

    /// One member of a fixture: its name, its stored bytes, whether it is a
    /// directory, and the host attribute word the header records.
    pub(crate) struct Entry<'a> {
        /// The member's name, with `\` as the separator RAR uses.
        pub name: &'a str,
        /// Its stored bytes.
        pub data: &'a [u8],
        /// Whether the header's directory flag is set.
        pub dir: bool,
        /// `ATTR`, which for a Unix host is a mode.
        pub attr: u32,
    }

    impl<'a> Entry<'a> {
        /// An ordinary stored file.
        pub(crate) fn file(name: &'a str, data: &'a [u8]) -> Self {
            Self {
                name,
                data,
                dir: false,
                attr: FILE_ATTR,
            }
        }

        /// A directory.
        pub(crate) fn dir(name: &'a str) -> Self {
            Self {
                name,
                data: &[],
                dir: true,
                attr: FILE_ATTR,
            }
        }
    }

    /// A RAR 4.x file header (type `0x74`) with the member stored, not packed.
    fn rar_file(entry: &Entry<'_>) -> Vec<u8> {
        // 2024-02-29 12:34:56 as a DOS timestamp.
        const FTIME: u32 = (44 << 25) | (2 << 21) | (29 << 16) | (12 << 11) | (34 << 5) | 28;
        // `LHD_LONG_BLOCK`, plus `LHD_DIRECTORY` in the window-size bits.
        let flags = 0x8000u16 | if entry.dir { 0x00e0 } else { 0 };
        let data: &[u8] = if entry.dir { &[] } else { entry.data };
        let name = entry.name.as_bytes();
        let mut body = Vec::new();
        let len = u32::try_from(data.len()).expect("a small fixture");
        body.extend_from_slice(&len.to_le_bytes()); // PACK_SIZE
        body.extend_from_slice(&len.to_le_bytes()); // UNP_SIZE
        body.push(3); // HOST_OS: Unix
        body.extend_from_slice(&crc32(data).to_le_bytes());
        body.extend_from_slice(&FTIME.to_le_bytes());
        body.push(20); // UNP_VER
        body.push(0x30); // METHOD: stored
        body.extend_from_slice(
            &u16::try_from(name.len())
                .expect("a short name")
                .to_le_bytes(),
        );
        body.extend_from_slice(&entry.attr.to_le_bytes()); // ATTR
        body.extend_from_slice(name);
        rar_block(0x74, flags, &body, data)
    }

    /// A whole RAR 4.x container: marker, main header, the members, terminator.
    pub(crate) fn write_rar(path: &Path, members: &[Entry<'_>]) {
        let mut out = b"Rar!\x1a\x07\x00".to_vec();
        let mut main = Vec::new();
        main.extend_from_slice(&0u16.to_le_bytes()); // HighPosAv
        main.extend_from_slice(&0u32.to_le_bytes()); // PosAv
        out.extend_from_slice(&rar_block(0x73, 0x0000, &main, &[]));
        for member in members {
            out.extend_from_slice(&rar_file(member));
        }
        out.extend_from_slice(&rar_block(0x7b, 0x4000, &[], &[]));
        std::fs::write(path, out).expect("write the fixture");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::archive::index::{Builder, Index};
    use std::sync::Arc;

    use super::fixture::{Entry, write_rar};

    fn temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hcmd-rar-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn index_of(path: &Path) -> Arc<Index> {
        let index = Arc::new(Index::new());
        {
            let mut sink = Builder::new(Arc::clone(&index), RarFormat.backslash_separators());
            RarFormat.index(path, &mut sink).expect("index");
        }
        index.finish(crate::vfs::archive::index::IndexStatus::Complete);
        index
    }

    #[test]
    fn members_are_listed_with_their_metadata_and_extracted() {
        let dir = temp("read");
        let path = dir.join("a.rar");
        let body: Vec<u8> = (0..=255u8).cycle().take(2048).collect();
        write_rar(
            &path,
            &[
                Entry::file("top.txt", b"top level"),
                Entry::dir("d"),
                // RAR stores `\` as its separator: this is `d/inner.bin`.
                Entry::file("d\\inner.bin", &body),
                Entry::file("d\\empty.txt", b""),
            ],
        );

        let index = index_of(&path);
        assert!(index.is_dir("d"), "a backslash is a separator in RAR");
        let top = index.get("top.txt").expect("top.txt");
        assert_eq!(top.size, 9);
        assert!(top.mtime.is_some(), "a RAR records a timestamp");
        assert_eq!(top.mode, 0, "unrar cannot say which host wrote the attrs");

        let inner = index.get("d/inner.bin").expect("d/inner.bin");
        assert_eq!(inner.size, 2048);

        let out = dir.join("out.bin");
        let written = RarFormat
            .extract_member(&path, &inner, &out)
            .expect("extract");
        assert_eq!(written, 2048);
        assert_eq!(std::fs::read(&out).expect("read back"), body);

        // And an empty member, which is the case a size-driven loop gets wrong.
        let empty = index.get("d/empty.txt").expect("d/empty.txt");
        assert_eq!(empty.size, 0);
        let out = dir.join("empty.out");
        assert_eq!(
            RarFormat
                .extract_member(&path, &empty, &out)
                .expect("extract the empty member"),
            0
        );
        assert!(std::fs::read(&out).expect("read back").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_escaping_member_is_never_listed() {
        // The RAR spelling of Zip Slip, with the format's own separator.
        let dir = temp("slip");
        let path = dir.join("evil.rar");
        write_rar(
            &path,
            &[
                Entry::file("..\\..\\etc\\passwd", b"root::0:0"),
                Entry::file("\\etc\\shadow", b"x"),
                Entry::file("safe.txt", b"ok"),
            ],
        );
        let index = index_of(&path);
        assert_eq!(index.len(), 1, "only safe.txt");
        assert!(index.get("safe.txt").is_some());
        assert_eq!(index.refusals().0, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_member_the_container_no_longer_has_is_reported() {
        let dir = temp("gone");
        let path = dir.join("a.rar");
        write_rar(&path, &[Entry::file("only.txt", b"one")]);
        let ghost = Member {
            path: "ghost.txt".to_string(),
            kind: MemberKind::File,
            size: 3,
            mtime: None,
            mode: 0,
            uid: 0,
            gid: 0,
            locator: Locator::Ordinal(9),
            synthetic: false,
        };
        let outcome = RarFormat.extract_member(&path, &ghost, &dir.join("x"));
        assert!(matches!(outcome, Err(Error::NotFound(_))), "{outcome:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_symlink_member_is_neither_listed_as_a_file_nor_reproduced() {
        // **The `.rar` symlink escape.** `libunrar` reproduces a member whose
        // attribute word says `S_IFLNK` by calling `symlink(2)` at the path it
        // is handed, and its own escape check counts the `..` in the target
        // against the depth of *that* path rather than against the archive's
        // root - so this member passes it and plants a link to a file the
        // archive does not contain. It was then indexed as an ordinary file,
        // extracted into the session cache, and read *through*: `Alt+F6`
        // copied the contents of an arbitrary readable file into the
        // destination and reported the job complete.
        let dir = temp("link");
        let path = dir.join("evil.rar");
        let target = b"../../../victim.txt";
        write_rar(
            &path,
            &[
                Entry::file("safe.txt", b"a legitimate member"),
                Entry {
                    name: "x\\y\\link",
                    data: target,
                    dir: false,
                    attr: super::fixture::SYMLINK_ATTR,
                },
            ],
        );

        let index = index_of(&path);
        let member = index.get("x/y/link").expect("the link is still listed");
        assert!(
            matches!(member.kind, MemberKind::Other),
            "not a file: nothing may read it as one ({:?})",
            member.kind
        );

        // And the library is never asked to reproduce it, so it never gets to
        // call `symlink(2)` on a path this program chose.
        let out = dir.join("planted");
        let outcome = RarFormat.extract_member(&path, &member, &out);
        assert!(outcome.is_err(), "refused: {outcome:?}");
        assert!(
            !std::fs::symlink_metadata(&out).is_ok(),
            "and nothing was created where it would have gone"
        );

        // The legitimate member is unaffected: a refusal is per entry.
        //
        let safe = index.get("safe.txt").expect("safe.txt");
        let out = dir.join("safe.out");
        assert_eq!(
            RarFormat
                .extract_member(&path, &safe, &out)
                .expect("extract"),
            19
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What is testable without a fixture: that the format is read-only, that
    /// it materialises, and that its separator rule is the exception.
    #[test]
    fn rar_is_read_only_and_materialised() {
        assert_eq!(RarFormat.id(), FormatId::Rar);
        assert_eq!(RarFormat.write_model(), WriteModel::None);
        assert_eq!(RarFormat.member_source(), MemberSource::Materialise);
        assert!(RarFormat.backslash_separators());
        let caps = RarFormat.capabilities();
        assert!(!caps.writable, "RAR is never written");
        assert!(caps.seekable, "a materialised member is a real file");
        assert!(!caps.can_execute);
    }

    #[test]
    fn a_file_that_is_not_a_rar_is_reported_not_panicked() {
        let dir = std::env::temp_dir().join(format!("hcmd-rar-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("not.rar");
        std::fs::write(&path, b"Rar!\x1a\x07\x00truncated nonsense").expect("write");

        let index = Arc::new(Index::new());
        let mut sink = Builder::new(Arc::clone(&index), false);
        let outcome = RarFormat.index(&path, &mut sink);
        assert!(outcome.is_err(), "a corrupt rar is an error");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dos_timestamps_convert_or_decline() {
        // 2024-02-29 12:34:56 → a real instant.
        let raw = (44u32 << 25) | (2 << 21) | (29 << 16) | (12 << 11) | (34 << 5) | 28;
        assert!(dos_time(raw).is_some());
        // Month 0 is not a month.
        let bad = (44u32 << 25) | (29 << 16);
        assert_eq!(dos_time(bad), None);
    }
}
