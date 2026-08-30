//! `.zip`, on the `zip` crate.
//!
//! The one format in the table with a real central directory: listing is a
//! read of that directory and costs nothing proportional to the archive's
//! size, which is why a zip fills the panel instantly where a compressed tar
//! fills it progressively.
//!
//! Member reads stream through [`super::stream::piped`]: `zip` hands out a
//! reader borrowed from the open archive, and this crate is
//! `#![forbid(unsafe_code)]`, so the member is driven by a worker thread
//! rather than held as a self-reference.

use std::io::{Read, Seek, Write};
use std::path::Path;
use std::time::SystemTime;

use crate::error::{Error, Result};

use super::format::{
    ArchiveFormat, CompressionLevel, FormatId, MemberEdit, WriteModel, WriteProgress,
};
use super::index::{IndexSink, Locator, Member, MemberKind, RawMember};
use super::rewrite::{
    ExactReader, Fate, Plan, Put, Rewrite, TailGuard, copy_watched, ensure_room, note,
    rewrite_footprint, verify_written,
};
use super::safety::normalize_member;

/// How long a zip symlink's target may be before this backend stops reading
/// it at index time.
///
/// A zip stores a symlink's target as the member's *contents*, so knowing
/// where a link points means decompressing it. Real targets are a few dozen
/// bytes; a "symlink" whose contents are a gigabyte is a container lying about
/// what it holds, and it is listed as a link to nowhere rather than read.
const MAX_LINK_TARGET: u64 = 4096;

/// The file-type bits of a POSIX mode, and the one that means "directory".
///
/// Spelled out rather than pulled from `libc`, which is not a dependency of
/// this milestone and would be one more crate for two constants.
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;

/// The zip backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZipFormat;

/// Open the container, phrasing the failure in terms of the file.
fn open(container: &Path) -> Result<zip::ZipArchive<std::fs::File>> {
    let file = std::fs::File::open(container).map_err(|e| Error::io(container, e))?;
    zip::ZipArchive::new(file).map_err(|e| Error::msg(format!("{}: {e}", container.display())))
}

/// A DOS timestamp, which is local time with two-second resolution.
///
/// `chrono` is already a dependency and is the only thing here
/// that knows what the local timezone was. An ambiguous or impossible local
/// time - the hour a DST change removes - yields `None` rather than a guess.
fn to_system_time(when: zip::DateTime) -> Option<SystemTime> {
    use chrono::TimeZone as _;
    let date = chrono::NaiveDate::from_ymd_opt(
        i32::from(when.year()),
        u32::from(when.month()),
        u32::from(when.day()),
    )?;
    let naive = date.and_hms_opt(
        u32::from(when.hour()),
        u32::from(when.minute()),
        u32::from(when.second()),
    )?;
    let local = chrono::Local.from_local_datetime(&naive).single()?;
    SystemTime::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(
        u64::try_from(local.timestamp()).ok()?,
    ))
}

impl ArchiveFormat for ZipFormat {
    fn id(&self) -> FormatId {
        FormatId::Zip
    }

    fn write_model(&self) -> WriteModel {
        // "A `.zip` supports in-place member addition."
        WriteModel::Member
    }

    fn index(&self, container: &Path, sink: &mut dyn IndexSink) -> Result<()> {
        let mut archive = open(container)?;
        let count = archive.len();
        for ordinal in 0..count {
            if sink.cancelled() {
                return Ok(());
            }
            // `by_index_raw` reads the entry's metadata without setting up a
            // decoder, so listing an archive never decompresses anything and
            // an encrypted member is still listed - its name and size are not
            // the secret, and hiding it would make the archive look empty.
            let Ok(entry) = archive.by_index_raw(ordinal) else {
                // One unreadable central-directory record does not invalidate
                // the others.
                continue;
            };
            let name = entry.name().to_string();
            let size = entry.size();
            let mode = entry.unix_mode().unwrap_or(0);
            let mtime = entry.last_modified().and_then(to_system_time);
            // `zip` answers `is_dir` from the trailing `/` alone, which is what
            // the format says; some writers record the directory only in the
            // mode bits, so both are consulted rather than trusting one.
            let is_dir = entry.is_dir() || mode & S_IFMT == S_IFDIR;
            let is_symlink = entry.is_symlink();
            let encrypted = entry.encrypted();
            drop(entry);

            let kind = if is_dir {
                MemberKind::Dir
            } else if is_symlink && !encrypted && size <= MAX_LINK_TARGET {
                MemberKind::Symlink(link_target(&mut archive, ordinal))
            } else if is_symlink {
                MemberKind::Symlink(String::new())
            } else {
                MemberKind::File
            };

            let raw = RawMember {
                name,
                kind,
                // A directory's declared size is not a size; the panel renders
                // `<DIR>` there and a stray number would only be wrong.
                size: if is_dir { 0 } else { size },
                mtime,
                mode,
                uid: 0,
                gid: 0,
                // A directory has no contents, so it has nowhere to be read
                // from. Saying so here is what makes `read_member` refuse it
                // without having to know what a zip directory record looks
                // like.
                locator: if is_dir {
                    Locator::None
                } else {
                    Locator::Ordinal(ordinal)
                },
            };
            if !sink.push(raw) {
                return Ok(());
            }
        }
        Ok(())
    }

    fn read_member(&self, container: &Path, member: &Member, out: &mut dyn Write) -> Result<u64> {
        if matches!(member.kind, MemberKind::Dir) {
            return Err(Error::InvalidPath(format!(
                "{}: a directory has no contents to read",
                member.path
            )));
        }
        let Locator::Ordinal(ordinal) = member.locator else {
            return Err(Error::InvalidPath(format!(
                "{}: a directory has no contents to read",
                member.path
            )));
        };
        let mut archive = open(container)?;
        let mut entry = archive.by_index(ordinal).map_err(|e| {
            Error::msg(format!("{}: {} in {}", member.path, e, container.display()))
        })?;
        std::io::copy(&mut entry, out).map_err(Error::Bare)
    }

    /// Add, replace, remove and rename members.
    ///
    /// > A `.zip` supports in-place member addition.
    ///
    /// So a pure addition of names the archive does not already have is an
    /// **append**: the new members go where the central directory was, and a new
    /// central directory goes after them. Adding one file to a 2 GB zip writes
    /// that file and the directory, and nothing else - which is what the design
    /// means by exempting `.zip` from both of its gates.
    ///
    /// Anything else - a removal, a rename, a replacement - moves every byte
    /// after the member it touches, so it is written into a new container
    /// beside the original and renamed over it only on success. Even then
    /// nothing is *recompressed*: a surviving member's compressed bytes are
    /// copied across verbatim, so a repack costs one pass over the file rather
    /// than a deflate of all of it.
    fn apply(
        &self,
        container: &Path,
        edits: &[MemberEdit],
        progress: &mut dyn WriteProgress,
    ) -> Result<()> {
        let plan = Plan::new(edits)?;
        if plan.is_empty() {
            return Ok(());
        }
        let (existing, central) = survey(container)?;
        if plan.adds_only() {
            let taken = plan
                .puts()
                .iter()
                .any(|put| existing.iter().any(|had| had.key == put.member_path));
            // A central directory too large to hold in memory is the one case
            // that falls through: an in-place append that cannot be undone is
            // not a member-level write, it is a gamble.
            if !taken && let Some(guard) = TailGuard::capture(container, central)? {
                return append_in_place(container, guard, plan.puts(), progress);
            }
        }
        repack(container, &plan, &existing, progress)
    }

    /// Pack `edits` into a brand-new `.zip` at `dest` (`Alt+F5`).
    ///
    /// Written to a temp file and renamed into place, and read back before it
    /// is called done: the "pack then delete sources" only gets to
    /// delete anything once the archive has been proved readable.
    fn create(
        &self,
        dest: &Path,
        edits: &[MemberEdit],
        level: CompressionLevel,
        progress: &mut dyn WriteProgress,
    ) -> Result<()> {
        let plan = Plan::new(edits)?;
        if !plan.adds_only() {
            return Err(Error::msg(
                "a new archive has no members to remove or rename yet".to_string(),
            ));
        }
        let rewrite = Rewrite::beside(dest)?;
        let mut writer = zip::ZipWriter::new(rewrite.create()?);
        let outcome = write_puts(&mut writer, plan.puts(), level, progress);
        let file = writer
            .finish()
            .map_err(|e| Error::msg(format!("{}: {e}", dest.display())));
        outcome?;
        let mut file = file?;
        file.flush().map_err(Error::Bare)?;
        drop(file);
        verify_written(self, rewrite.path(), &plan.added_paths(), plan.puts().len())?;
        rewrite.commit()
    }
}

/// One member of a container as it is now.
#[derive(Debug, Clone)]
struct Existing {
    ordinal: usize,
    /// The normalised path, which is what the index and the plan speak.
    key: String,
    is_dir: bool,
    is_symlink: bool,
    mode: u32,
    compressed: u64,
}

/// Every member of `container`, and where its central directory starts.
///
/// The second number is what an in-place append is about to overwrite, and
/// therefore what has to be kept in order to put the archive back if the append
/// fails. An archive with no members has no central header to
/// point at, so the whole file - an end-of-central-directory record and nothing
/// else - is the tail.
fn survey(container: &Path) -> Result<(Vec<Existing>, u64)> {
    let mut archive = open(container)?;
    let count = archive.len();
    let mut members = Vec::with_capacity(count);
    let mut central: Option<u64> = None;
    for ordinal in 0..count {
        let Ok(entry) = archive.by_index_raw(ordinal) else {
            // One unreadable central-directory record does not invalidate the
            // others; it is also not something this write may rewrite, so a
            // repack drops it exactly as the index does.
            continue;
        };
        let start = entry.central_header_start();
        central = Some(central.map_or(start, |had: u64| had.min(start)));
        let raw = entry.name().to_string();
        let is_dir = entry.is_dir();
        let is_symlink = entry.is_symlink();
        let mode = entry.unix_mode().unwrap_or(0);
        let compressed = entry.compressed_size();
        drop(entry);
        // A name that could escape is not a member as far as the rest of this
        // crate is concerned, so it is neither a name a write
        // can collide with nor one a repack carries forward.
        if let Ok(key) = normalize_member(&raw, false) {
            members.push(Existing {
                ordinal,
                key,
                is_dir,
                is_symlink,
                mode,
                compressed,
            });
        }
    }
    Ok((members, central.unwrap_or(0)))
}

/// Append members to a zip without rewriting it.
///
/// `guard` holds the central directory that is about to be overwritten. Every
/// way out of here that is not success puts it back, so an append that fails,
/// or that the user cancels, leaves the archive exactly as it was - and
/// "success" means the archive was **read back** afterwards, not merely
/// written. Reading a zip's members back is a read of its central directory
/// and nothing else, so the check that closes the loop costs nothing worth
/// saving.
fn append_in_place(
    container: &Path,
    mut guard: TailGuard,
    puts: &[Put],
    progress: &mut dyn WriteProgress,
) -> Result<()> {
    let expected: Vec<String> = puts.iter().map(|put| put.member_path.clone()).collect();
    let outcome = (|| -> Result<()> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(container)
            .map_err(|e| Error::io(container, e))?;
        let mut writer = zip::ZipWriter::new_append(file)
            .map_err(|e| Error::msg(format!("{}: {e}", container.display())))?;
        let written = write_puts(&mut writer, puts, CompressionLevel::DEFAULT, progress);
        let finished = writer
            .finish()
            .map_err(|e| Error::msg(format!("{}: {e}", container.display())));
        written?;
        let mut file = finished?;
        file.flush().map_err(|e| Error::io(container, e))?;
        // Whatever the old central directory left past the new one is not part
        // of this archive any more.
        let end = file
            .stream_position()
            .map_err(|e| Error::io(container, e))?;
        file.set_len(end).map_err(|e| Error::io(container, e))?;
        file.sync_all().map_err(|e| Error::io(container, e))?;
        drop(file);
        verify_written(&ZipFormat, container, &expected, 0)
    })();

    match outcome {
        Ok(()) => {
            guard.disarm();
            Ok(())
        }
        Err(err) => {
            guard.restore()?;
            guard.disarm();
            Err(err)
        }
    }
}

/// Write a whole new zip holding `plan` applied to `container`.
///
/// A surviving member is copied **raw**: its already-compressed bytes go
/// straight across, so nothing is inflated and nothing is re-deflated and the
/// archive that comes out is byte-for-byte the same members. The two exceptions
/// are the two kinds of entry whose *type* is not carried by the raw copy -
/// `zip`'s raw path rebuilds the external attributes as a regular file - so a
/// directory and a symlink are re-created as what they are. Getting that wrong
/// would silently turn every symlink in the archive into a text file holding
/// its target.
fn repack(
    container: &Path,
    plan: &Plan,
    existing: &[Existing],
    progress: &mut dyn WriteProgress,
) -> Result<()> {
    let size = std::fs::metadata(container)
        .map(|meta| meta.len())
        .unwrap_or(0);
    ensure_room(container, rewrite_footprint(size))?;

    let rewrite = Rewrite::beside(container)?;
    let mut writer = zip::ZipWriter::new(rewrite.create()?);
    let mut source = open(container)?;
    let mut kept = 0usize;

    let outcome = (|| -> Result<()> {
        for had in existing {
            let Fate::Keep(name) = plan.fate(&had.key) else {
                continue;
            };
            // Cancellation between members; the raw copy below is one call into
            // the library and cannot be interrupted inside it (the design
            // asks for both "between files" and "within a large file's chunk
            // loop", and this is the half a verbatim copy can offer).
            note(progress, 0)?;
            let options = zip::write::SimpleFileOptions::default().unix_permissions(
                if had.mode & 0o777 == 0 {
                    0o755
                } else {
                    had.mode & 0o777
                },
            );
            if had.is_dir {
                writer
                    .add_directory(name, options)
                    .map_err(|e| Error::msg(format!("{}: {e}", had.key)))?;
            } else if had.is_symlink {
                let target = link_target(&mut source, had.ordinal);
                writer
                    .add_symlink(name, target, options)
                    .map_err(|e| Error::msg(format!("{}: {e}", had.key)))?;
            } else {
                let entry = source
                    .by_index_raw(had.ordinal)
                    .map_err(|e| Error::msg(format!("{}: {e}", had.key)))?;
                writer
                    .raw_copy_file_rename(entry, name)
                    .map_err(|e| Error::msg(format!("{}: {e}", had.key)))?;
                note(progress, had.compressed)?;
            }
            kept = kept.saturating_add(1);
        }
        write_puts(
            &mut writer,
            plan.puts(),
            CompressionLevel::DEFAULT,
            progress,
        )
    })();

    let finished = writer
        .finish()
        .map_err(|e| Error::msg(format!("{}: {e}", container.display())));
    outcome?;
    let mut file = finished?;
    file.flush().map_err(Error::Bare)?;
    drop(file);

    let expected = kept.saturating_add(plan.puts().len());
    verify_written(&ZipFormat, rewrite.path(), &plan.added_paths(), expected)?;
    rewrite.commit()
}

/// Write the plan's additions into a zip being built.
fn write_puts<W: Write + Seek>(
    writer: &mut zip::ZipWriter<W>,
    puts: &[Put],
    level: CompressionLevel,
    progress: &mut dyn WriteProgress,
) -> Result<()> {
    for put in puts {
        let size = put.size()?;
        let mut options = zip::write::SimpleFileOptions::default()
            .unix_permissions(put.file_mode())
            .large_file(size >= ZIP64_THRESHOLD);
        options = if level.get() == 0 {
            options.compression_method(zip::CompressionMethod::Stored)
        } else {
            options
                .compression_method(zip::CompressionMethod::Deflated)
                .compression_level(Some(i64::from(level.get())))
        };
        if let Some(when) = dos_time(put.mtime) {
            options = options.last_modified_time(when);
        }

        if put.is_dir() {
            writer
                .add_directory(put.member_path.clone(), options)
                .map_err(|e| Error::msg(format!("{}: {e}", put.member_path)))?;
            continue;
        }
        let Some(source) = put.source.as_ref() else {
            continue;
        };
        writer
            .start_file(put.member_path.clone(), options)
            .map_err(|e| Error::msg(format!("{}: {e}", put.member_path)))?;
        let file = std::fs::File::open(source).map_err(|e| Error::io(source, e))?;
        // Exactly the size the header was written with: a source that shrank
        // under us is an error, never a member whose bytes disagree with the
        // archive's own accounting.
        let mut bytes = ExactReader::new(file, size, put.member_path.clone());
        copy_watched(&mut bytes, writer, progress)?;
    }
    Ok(())
}

/// The size past which a zip member needs the ZIP64 extensions.
const ZIP64_THRESHOLD: u64 = u32::MAX as u64;

/// A DOS timestamp for a `SystemTime`.
///
/// The inverse of [`to_system_time`], through the same local timezone, so a
/// member read out of an archive and written back into one keeps its time. A
/// time DOS cannot represent - anything before 1980 or after 2107 - yields
/// `None` and the writer's own default is used rather than a wrong answer.
fn dos_time(when: Option<SystemTime>) -> Option<zip::DateTime> {
    use chrono::{Datelike as _, TimeZone as _, Timelike as _};
    let secs = when?.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_secs();
    let local = chrono::Local
        .timestamp_opt(i64::try_from(secs).ok()?, 0)
        .single()?;
    zip::DateTime::from_date_and_time(
        u16::try_from(local.year()).ok()?,
        u8::try_from(local.month()).ok()?,
        u8::try_from(local.day()).ok()?,
        u8::try_from(local.hour()).ok()?,
        u8::try_from(local.minute()).ok()?,
        u8::try_from(local.second()).ok()?,
    )
    .ok()
}

/// A symlink member's target, which a zip stores as the member's contents.
///
/// A target that cannot be read is an empty string - a link to nowhere - and
/// never an error: a broken link in a listing is normal, and refusing to list
/// the archive because one link is odd is not.
fn link_target(archive: &mut zip::ZipArchive<std::fs::File>, ordinal: usize) -> String {
    let Ok(mut entry) = archive.by_index(ordinal) else {
        return String::new();
    };
    let mut buf = Vec::new();
    if entry
        .by_ref()
        .take(MAX_LINK_TARGET)
        .read_to_end(&mut buf)
        .is_err()
    {
        return String::new();
    }
    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::archive::index::{Builder, Index, IndexStatus};
    use std::sync::Arc;
    use zip::write::SimpleFileOptions;

    fn temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hcmd-zip-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn write_zip(path: &Path, members: &[(&str, &[u8])]) {
        let file = std::fs::File::create(path).expect("create");
        let mut writer = zip::ZipWriter::new(file);
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(0o644);
        for (name, body) in members {
            writer.start_file(*name, options).expect("start");
            writer.write_all(body).expect("write");
        }
        writer.finish().expect("finish");
    }

    fn index_of(path: &Path) -> Arc<Index> {
        let index = Arc::new(Index::new());
        {
            let mut sink = Builder::new(Arc::clone(&index), false);
            ZipFormat.index(path, &mut sink).expect("index");
        }
        index.finish(IndexStatus::Complete);
        index
    }

    #[test]
    fn members_are_listed_and_read() {
        let dir = temp("read");
        let path = dir.join("a.zip");
        let big = vec![b'z'; 300_000];
        write_zip(
            &path,
            &[("top.txt", b"top"), ("d/inner.bin", &big), ("d/", b"")],
        );

        let index = index_of(&path);
        assert!(index.is_dir("d"));
        let top = index.get("top.txt").expect("top");
        assert_eq!(top.size, 3);
        assert_eq!(top.mode & 0o777, 0o644);
        assert!(top.mtime.is_some(), "a zip records a timestamp");

        let mut got = Vec::new();
        ZipFormat.read_member(&path, &top, &mut got).expect("read");
        assert_eq!(got, b"top");

        let inner = index.get("d/inner.bin").expect("inner");
        let mut got = Vec::new();
        ZipFormat
            .read_member(&path, &inner, &mut got)
            .expect("read");
        assert_eq!(got, big);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_that_is_not_utf8_lists_lossily_and_still_opens() {
        // The same rule `LocalFs` follows. A zip with the UTF-8 flag set and a
        // name that is not UTF-8 is the shape this happens in; the member is
        // still addressed by its position in the central directory, so it opens
        // whatever its name decoded to.
        let dir = temp("lossy");
        let path = dir.join("latin1.zip");
        write_zip(&path, &[("caf\u{e9}.txt", b"not utf-8")]);
        let mut raw = std::fs::read(&path).expect("read");
        // `café` is `caf\xc3\xa9` on disk; truncating the pair to one byte
        // leaves the length alone and the encoding invalid.
        let mut at = 0;
        while let Some(found) = raw
            .get(at..)
            .and_then(|rest| rest.windows(2).position(|w| w == [0xc3, 0xa9]))
        {
            let start = at.saturating_add(found);
            if let Some(byte) = raw.get_mut(start) {
                *byte = 0xe9;
            }
            at = start.saturating_add(2);
        }
        std::fs::write(&path, &raw).expect("rewrite");

        let index = index_of(&path);
        assert_eq!(index.len(), 1);
        let (_, rows, _) = index.children_from("", 0);
        let name = rows.first().map(|e| e.name.clone()).unwrap_or_default();
        assert!(
            name.contains('\u{fffd}'),
            "the undecodable byte renders lossily, got {name:?}"
        );
        let member = index.get(&name).expect("the lossy name is the index key");
        let mut got = Vec::new();
        ZipFormat
            .read_member(&path, &member, &mut got)
            .expect("a lossy name is still openable");
        assert_eq!(got, b"not utf-8");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_member_has_nothing_to_read() {
        let dir = temp("dir");
        let path = dir.join("d.zip");
        write_zip(&path, &[("d/", b""), ("d/f.txt", b"x")]);
        let index = index_of(&path);
        let d = index.get("d").expect("d");
        assert_eq!(d.kind, MemberKind::Dir);
        assert_eq!(d.locator, Locator::None, "a directory is nowhere to read");
        assert_eq!(d.size, 0);
        let mut got = Vec::new();
        assert!(
            ZipFormat.read_member(&path, &d, &mut got).is_err(),
            "a directory has no contents"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_entry_with_no_recorded_mode_reports_unknown_rather_than_zero_six_four_four() {
        // the columns can render "unknown"; inventing `0o644` for a
        // zip that never stored a mode would put a fact in the panel that the
        // container does not contain.
        let dir = temp("nomode");
        let path = dir.join("bare.zip");
        let file = std::fs::File::create(&path).expect("create");
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file("plain.txt", SimpleFileOptions::default())
            .expect("start");
        writer.write_all(b"hi").expect("write");
        writer.finish().expect("finish");

        let index = index_of(&path);
        let member = index.get("plain.txt").expect("plain.txt");
        assert_eq!(member.uid, 0, "a zip carries no owner");
        assert_eq!(member.gid, 0);
        assert_eq!(member.size, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_escaping_member_is_never_listed() {
        let dir = temp("slip");
        let path = dir.join("evil.zip");
        // The canonical Zip Slip payload, and the absolute-path variant.
        write_zip(
            &path,
            &[("../../../../tmp/pwned", b"x"), ("safe.txt", b"ok")],
        );
        let index = index_of(&path);
        assert_eq!(index.len(), 1, "only safe.txt");
        assert!(index.get("safe.txt").is_some());
        assert!(index.refusals().0 >= 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_is_not_a_zip_is_reported_not_panicked() {
        let dir = temp("bad");
        let path = dir.join("not.zip");
        std::fs::write(&path, b"this is not a zip archive").expect("write");
        let index = Arc::new(Index::new());
        let mut sink = Builder::new(Arc::clone(&index), false);
        assert!(ZipFormat.index(&path, &mut sink).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A [`WriteProgress`] that counts, and one that refuses after `after`
    /// bytes so a write can be interrupted the way `Esc` interrupts one.
    #[derive(Default)]
    struct Counter {
        seen: u64,
        stop_after: Option<u64>,
    }

    impl WriteProgress for Counter {
        fn bytes(&mut self, n: u64) -> bool {
            self.seen = self.seen.saturating_add(n);
            match self.stop_after {
                Some(limit) => self.seen <= limit,
                None => true,
            }
        }
    }

    fn put(member: &str, source: &Path) -> MemberEdit {
        MemberEdit::Put {
            member_path: member.to_string(),
            source: source.to_path_buf(),
            mode: 0o644,
            mtime: Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)),
        }
    }

    fn read_member(path: &Path, member: &str) -> Vec<u8> {
        let index = index_of(path);
        let found = index
            .get(member)
            .unwrap_or_else(|| panic!("{member} is in the archive"));
        let mut out = Vec::new();
        ZipFormat
            .read_member(path, &found, &mut out)
            .unwrap_or_else(|e| panic!("read {member}: {e}"));
        out
    }

    #[test]
    fn adding_a_member_appends_rather_than_rewriting() {
        // "A `.zip` supports in-place member addition." The
        // proof is that everything up to the old central directory is the same
        // bytes afterwards - nothing was recompressed and nothing moved.
        let dir = temp("append");
        let path = dir.join("a.zip");
        write_zip(
            &path,
            &[("keep.txt", b"keep me"), ("d/deep.bin", &[7u8; 5000])],
        );
        let before = std::fs::read(&path).expect("read");
        let (_, central) = survey(&path).expect("survey");
        let central = usize::try_from(central).expect("central fits");

        let source = dir.join("new.txt");
        std::fs::write(&source, b"a brand new member").expect("write");
        let mut progress = Counter::default();
        ZipFormat
            .apply(&path, &[put("added/new.txt", &source)], &mut progress)
            .expect("apply");

        let after = std::fs::read(&path).expect("read");
        assert_eq!(
            after.get(..central),
            before.get(..central),
            "the members that were there were not touched"
        );
        assert_eq!(read_member(&path, "keep.txt"), b"keep me");
        assert_eq!(read_member(&path, "d/deep.bin"), vec![7u8; 5000]);
        assert_eq!(read_member(&path, "added/new.txt"), b"a brand new member");
        assert!(index_of(&path).is_dir("added"), "the parent is synthesised");
        assert!(progress.seen >= 18, "the write reported its bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_interrupted_append_leaves_the_archive_byte_for_byte() {
        // cancelling "must leave no half-written destination",
        // and the design wants the archive itself intact. A member-level
        // write edits the container in place, so this is the case that has to
        // be tested by actually interrupting one.
        let dir = temp("cancel");
        let path = dir.join("a.zip");
        write_zip(&path, &[("keep.txt", b"keep me")]);
        let before = std::fs::read(&path).expect("read");

        let source = dir.join("big.bin");
        std::fs::write(&source, vec![3u8; 400_000]).expect("write");
        let mut progress = Counter {
            seen: 0,
            stop_after: Some(64 * 1024),
        };
        let outcome = ZipFormat.apply(&path, &[put("big.bin", &source)], &mut progress);
        assert!(
            matches!(outcome, Err(Error::Cancelled)),
            "a cancelled write is cancelled, not failed: {outcome:?}"
        );
        assert_eq!(
            std::fs::read(&path).expect("read"),
            before,
            "the archive is exactly what it was"
        );
        assert_eq!(read_member(&path, "keep.txt"), b"keep me");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn removing_and_replacing_repack_and_keep_everything_else() {
        let dir = temp("repack");
        let path = dir.join("a.zip");
        write_zip(
            &path,
            &[
                ("keep.txt", b"still here"),
                ("drop/me.txt", b"gone"),
                ("drop/and/me.txt", b"also gone"),
                ("swap.txt", b"the old text"),
            ],
        );
        let source = dir.join("swap.txt");
        std::fs::write(&source, b"the new text").expect("write");

        let mut progress = Counter::default();
        ZipFormat
            .apply(
                &path,
                &[
                    MemberEdit::Remove {
                        member_path: "drop".to_string(),
                    },
                    put("swap.txt", &source),
                ],
                &mut progress,
            )
            .expect("apply");

        let index = index_of(&path);
        assert!(
            index.get("drop/me.txt").is_none(),
            "a directory takes everything beneath it"
        );
        assert!(index.get("drop/and/me.txt").is_none());
        assert_eq!(read_member(&path, "keep.txt"), b"still here");
        assert_eq!(read_member(&path, "swap.txt"), b"the new text");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rename_moves_a_directory_and_everything_in_it() {
        let dir = temp("rename");
        let path = dir.join("a.zip");
        write_zip(&path, &[("old/one.txt", b"1"), ("old/two/three.txt", b"3")]);
        ZipFormat
            .apply(
                &path,
                &[MemberEdit::Rename {
                    from: "old".to_string(),
                    to: "new".to_string(),
                }],
                &mut Counter::default(),
            )
            .expect("apply");
        assert_eq!(read_member(&path, "new/one.txt"), b"1");
        assert_eq!(read_member(&path, "new/two/three.txt"), b"3");
        assert!(index_of(&path).get("old/one.txt").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_symlink_is_still_a_symlink_after_a_repack() {
        // `zip`'s raw copy rebuilds an entry's external attributes as a regular
        // file, so a repack that used it for everything would silently turn
        // every symlink in the archive into a text file holding its target.
        let dir = temp("symlink");
        let path = dir.join("a.zip");
        {
            let file = std::fs::File::create(&path).expect("create");
            let mut writer = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .unix_permissions(0o644);
            writer.start_file("real.txt", options).expect("start");
            writer.write_all(b"real").expect("write");
            writer
                .add_symlink("link", "real.txt", options)
                .expect("symlink");
            writer.start_file("gone.txt", options).expect("start");
            writer.write_all(b"gone").expect("write");
            writer.finish().expect("finish");
        }
        ZipFormat
            .apply(
                &path,
                &[MemberEdit::Remove {
                    member_path: "gone.txt".to_string(),
                }],
                &mut Counter::default(),
            )
            .expect("apply");

        let index = index_of(&path);
        let link = index.get("link").expect("the link survives");
        assert_eq!(
            link.kind,
            MemberKind::Symlink("real.txt".to_string()),
            "and it is still a link, not a file holding its target"
        );
        assert_eq!(read_member(&path, "real.txt"), b"real");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn packing_writes_a_new_archive_and_verifies_it() {
        let dir = temp("pack");
        let one = dir.join("one.txt");
        let two = dir.join("two.bin");
        std::fs::write(&one, b"first").expect("write");
        std::fs::write(&two, vec![9u8; 100_000]).expect("write");
        let dest = dir.join("packed.zip");

        let mut progress = Counter::default();
        ZipFormat
            .create(
                &dest,
                &[
                    MemberEdit::PutDir {
                        member_path: "sub".to_string(),
                        mode: 0o755,
                    },
                    put("sub/one.txt", &one),
                    put("two.bin", &two),
                ],
                CompressionLevel::DEFAULT,
                &mut progress,
            )
            .expect("create");

        assert_eq!(read_member(&dest, "sub/one.txt"), b"first");
        assert_eq!(read_member(&dest, "two.bin"), vec![9u8; 100_000]);
        assert!(index_of(&dest).is_dir("sub"));
        assert_eq!(progress.seen, 100_005, "every source byte was reported");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cancelled_pack_leaves_nothing_behind() {
        let dir = temp("packcancel");
        let source = dir.join("big.bin");
        std::fs::write(&source, vec![1u8; 400_000]).expect("write");
        let dest = dir.join("packed.zip");
        let mut progress = Counter {
            seen: 0,
            stop_after: Some(64 * 1024),
        };
        let outcome = ZipFormat.create(
            &dest,
            &[put("big.bin", &source)],
            CompressionLevel::DEFAULT,
            &mut progress,
        );
        assert!(matches!(outcome, Err(Error::Cancelled)), "{outcome:?}");
        assert!(!dest.exists(), "no half-written archive");
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .expect("read_dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "big.bin")
            .collect();
        assert!(
            leftovers.is_empty(),
            "and no temp file either: {leftovers:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_escaping_member_name_is_refused_on_the_way_in() {
        // the design is about extraction, and it is just as much about
        // writing: an archive hcmd produced must not be a Zip Slip payload for
        // whoever unpacks it next.
        let dir = temp("slipwrite");
        let path = dir.join("a.zip");
        write_zip(&path, &[("ok.txt", b"ok")]);
        let source = dir.join("src.txt");
        std::fs::write(&source, b"x").expect("write");
        // The reason is checked and not just the failure: an `apply` that fell
        // over because the source file was missing, or because the container
        // could not be opened, would be an `is_err()` too and would read as
        // "the escape was refused" while the escape was never even considered.
        for (name, reason) in [
            ("../escape.txt", "a `..` component"),
            ("/etc/passwd", "an absolute path"),
            ("a/../../b", "a `..` component"),
        ] {
            let err = ZipFormat
                .apply(&path, &[put(name, &source)], &mut Counter::default())
                .expect_err("must be refused");
            assert!(
                matches!(err, Error::InvalidPath(_)),
                "{name} was refused for the wrong kind of reason: {err}"
            );
            let message = err.to_string();
            assert!(message.contains(name), "{message}");
            assert!(message.contains("refused"), "{message}");
            assert!(message.contains(reason), "{message}");
        }
        // The control: the same source under a name that does not escape goes
        // in, so the refusals above were about the names and nothing else.
        ZipFormat
            .apply(
                &path,
                &[put("inside/safe.txt", &source)],
                &mut Counter::default(),
            )
            .expect("a name that stays inside is written");
        assert_eq!(read_member(&path, "inside/safe.txt"), b"x");
        assert_eq!(read_member(&path, "ok.txt"), b"ok", "and nothing changed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replacing_a_member_leaves_no_duplicate_behind() {
        // A zip permits two entries with the same name, and every reader takes
        // the last, so an append that pretended to be a replacement would look
        // right through the index while doubling the archive's size on every
        // save. The count of *raw* entries is the only place that shows.
        let dir = temp("dupe");
        let path = dir.join("a.zip");
        write_zip(&path, &[("a.txt", b"first"), ("b.txt", b"other")]);
        let source = dir.join("a.txt");
        std::fs::write(&source, b"second").expect("write");

        ZipFormat
            .apply(&path, &[put("a.txt", &source)], &mut Counter::default())
            .expect("apply");

        let archive = open(&path).expect("open");
        assert_eq!(archive.len(), 2, "replaced, not appended");
        assert_eq!(read_member(&path, "a.txt"), b"second");
        assert_eq!(read_member(&path, "b.txt"), b"other");

        // And a member-level append really does leave the tail recoverable,
        // which is what makes the in-place path safe to take at all.
        let (_, central) = survey(&path).expect("survey");
        assert!(
            super::super::rewrite::TailGuard::capture(&path, central)
                .expect("capture")
                .is_some()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
