//! `.7z`, on `sevenz-rust2`.
//!
//! 7z carries a header listing every entry, so indexing costs a header read
//! and no decompression: [`sevenz_rust2::Archive::open`] parses it and nothing
//! else.
//!
//! # Solid archives
//!
//! 7z's compression blocks may span several members ("solid"), and a member in
//! the middle of a block can only be produced by decoding everything before it
//! in that block. `sevenz-rust2` says so in as many words, and there is no
//! trick that avoids it - it is what solid compression *is*. Reading one small
//! file out of a solid archive therefore costs the block, and reading the
//! members of a directory one after another costs it repeatedly. That is
//! stated in the design rather than hidden: the alternative would
//! be caching decoded blocks, which is memory this milestone has not agreed to
//! spend.

use std::io::{Read, Seek, Write};
use std::path::Path;
use std::time::SystemTime;

use crate::error::{Error, Result};

use super::format::{
    ArchiveFormat, CompressionLevel, FormatId, MemberEdit, WriteModel, WriteProgress,
};
use super::index::{IndexSink, Locator, Member, MemberKind, RawMember};
use super::rewrite::{
    ExactReader, Fate, Plan, Put, Rewrite, WatchedReader, ensure_room, note, rewrite_footprint,
    unwrap_cancellation, verify_written,
};
use super::safety::normalize_member;

/// p7zip's marker for "the high sixteen bits of the attribute word are a Unix
/// mode".
const FILE_ATTRIBUTE_UNIX_EXTENSION: u32 = 0x8000;

/// The 7z backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SevenZFormat;

/// The Unix mode 7z recorded, if it recorded one.
///
/// p7zip stores `st_mode` in the top half of the Windows attribute word and
/// sets bit 15 to say so. Without that bit there is no mode here at all, and
/// `0` is the honest answer rather than a fabricated `0o644`.
fn mode_of(entry: &sevenz_rust2::ArchiveEntry) -> u32 {
    if !entry.has_windows_attributes {
        return 0;
    }
    if entry.windows_attributes & FILE_ATTRIBUTE_UNIX_EXTENSION == 0 {
        return 0;
    }
    entry.windows_attributes >> 16
}

/// The modification time, when the entry has one.
fn mtime_of(entry: &sevenz_rust2::ArchiveEntry) -> Option<SystemTime> {
    entry
        .has_last_modified_date
        .then(|| SystemTime::from(entry.last_modified_date))
}

impl ArchiveFormat for SevenZFormat {
    fn id(&self) -> FormatId {
        FormatId::SevenZ
    }

    fn write_model(&self) -> WriteModel {
        // the design says `.7z` is writable; the design names only the
        // compressed tars as full rewrites, and says nothing about 7z. It is
        // classed as a rewrite here because `sevenz-rust2` offers no in-place
        // member insertion and 7z's own container shape - one header written
        // after the compressed blocks - does not admit one. That means the
        // size and free-space gates apply to it too, which is the
        // conservative reading of a silence. Recorded in the design as an
        // ambiguity in.
        WriteModel::FullRewrite
    }

    fn can_create(&self) -> bool {
        true
    }

    fn index(&self, container: &Path, sink: &mut dyn IndexSink) -> Result<()> {
        let archive = sevenz_rust2::Archive::open(container)
            .map_err(|e| Error::msg(format!("{}: {e}", container.display())))?;
        for (ordinal, entry) in archive.files.iter().enumerate() {
            if sink.cancelled() {
                return Ok(());
            }
            let raw = RawMember {
                name: entry.name.clone(),
                kind: if entry.is_directory {
                    MemberKind::Dir
                } else {
                    MemberKind::File
                },
                size: entry.size,
                mtime: mtime_of(entry),
                mode: mode_of(entry),
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

    fn read_member(&self, container: &Path, member: &Member, out: &mut dyn Write) -> Result<u64> {
        if matches!(member.kind, MemberKind::Dir) {
            return Err(Error::InvalidPath(format!(
                "{}: a directory has no contents to read",
                member.path
            )));
        }
        let Locator::Ordinal(wanted) = member.locator else {
            return Err(Error::InvalidPath(format!(
                "{}: a directory has no contents to read",
                member.path
            )));
        };

        let mut file = std::fs::File::open(container).map_err(|e| Error::io(container, e))?;
        let password = sevenz_rust2::Password::empty();
        let archive = sevenz_rust2::Archive::read(&mut file, &password)
            .map_err(|e| Error::msg(format!("{}: {e}", container.display())))?;

        // The index recorded a position in `archive.files`, so the member is
        // found by that position and never by its name: a container may hold
        // two entries whose names normalise to the same path, and the one the
        // panel is showing is the one at this ordinal.
        let entry = archive.files.get(wanted).ok_or_else(|| {
            Error::NotFound(format!("{} in {}", member.path, container.display()))
        })?;
        if entry.is_directory {
            return Err(Error::InvalidPath(format!(
                "{}: a directory has no contents to read",
                member.path
            )));
        }
        // An entry with no stream is an empty file. It is not in any block, so
        // there is nothing to decode and nothing to look for.
        let Some(block) = archive
            .stream_map
            .file_block_index
            .get(wanted)
            .copied()
            .flatten()
        else {
            return Ok(0);
        };
        let first = *archive
            .stream_map
            .block_first_file_index
            .get(block)
            .ok_or_else(|| {
                Error::msg(format!(
                    "{}: {} names a compression block it does not have",
                    container.display(),
                    member.path
                ))
            })?;

        // One block, not the archive. `ArchiveReader::for_each_entries` walks
        // every block from the first to the last whatever the callback answers,
        // which on a large 7z means decoding the whole container to read one
        // member; decoding the member's own block is the least that is correct
        // (a solid block still has to be decoded from its start - that is what
        // solid compression is).
        let mut at = first;
        let mut written = 0u64;
        let mut found = false;
        let mut failure: Option<Error> = None;
        sevenz_rust2::BlockDecoder::new(1, block, &archive, &password, &mut file)
            .for_each_entries(&mut |_entry, entry_reader| {
                let here = at;
                at = at.saturating_add(1);
                if here != wanted {
                    return Ok(true);
                }
                found = true;
                match std::io::copy(entry_reader, out) {
                    Ok(n) => written = n,
                    Err(e) => failure = Some(Error::Bare(e)),
                }
                Ok(false)
            })
            .map_err(|e| Error::msg(format!("{}: {e}", member.path)))?;

        if let Some(err) = failure {
            return Err(err);
        }
        if !found {
            return Err(Error::NotFound(format!(
                "{} in {}",
                member.path,
                container.display()
            )));
        }
        Ok(written)
    }

    /// Add, replace, remove and rename members.
    ///
    /// Always a **full rewrite**. 7z writes one header describing every block
    /// *after* the blocks themselves, and `sevenz-rust2` has no insertion API,
    /// so there is no member-level edit to be had here - see
    /// [`SevenZFormat::write_model`]. The new container is written beside the
    /// original, read back, and renamed over it only then; a failure or a
    /// cancellation leaves the archive exactly as it was.
    ///
    /// Two things about the result are worth stating rather than discovering:
    ///
    /// * a member that survives is **re-compressed**, because the only way to
    ///   read one out of a 7z is to decode its block. A zip repack copies
    ///   compressed bytes across; this one cannot.
    /// * the result is **non-solid** - one block per member. Solid blocks are
    ///   what make a 7z small and what make reading one member out of it cost
    ///   the whole block; `sevenz-rust2` builds them from a batch of entries
    ///   held together, which is memory proportional to the batch. A rewrite
    ///   that trades some ratio for a bounded footprint is the trade this
    ///   milestone is willing to make.
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
        let size = std::fs::metadata(container)
            .map(|meta| meta.len())
            .unwrap_or(0);
        ensure_room(container, rewrite_footprint(size))?;

        let rewrite = Rewrite::beside(container)?;
        let mut writer = sevenz_rust2::ArchiveWriter::new(rewrite.create()?)
            .map_err(|e| Error::msg(format!("{}: {e}", rewrite.path().display())))?;
        writer.set_content_methods(content_methods(CompressionLevel::DEFAULT));

        let mut kept = 0usize;
        {
            let mut reader =
                sevenz_rust2::ArchiveReader::open(container, sevenz_rust2::Password::empty())
                    .map_err(|e| Error::msg(format!("{}: {e}", container.display())))?;
            let mut failure: Option<Error> = None;
            reader
                .for_each_entries(|entry, body| {
                    match transfer_entry(&mut writer, entry, body, &plan, progress) {
                        Ok(true) => {
                            kept = kept.saturating_add(1);
                            Ok(true)
                        }
                        Ok(false) => Ok(true),
                        Err(err) => {
                            failure = Some(err);
                            Ok(false)
                        }
                    }
                })
                .map_err(|e| Error::msg(format!("{}: {e}", container.display())))?;
            if let Some(err) = failure {
                return Err(err);
            }
        }
        append_puts(&mut writer, plan.puts(), progress)?;

        let mut file = writer.finish().map_err(Error::Bare)?;
        file.flush().map_err(Error::Bare)?;
        drop(file);
        let expected = kept.saturating_add(plan.puts().len());
        verify_written(self, rewrite.path(), &plan.added_paths(), expected)?;
        rewrite.commit()
    }

    /// Pack `edits` into a brand-new `.7z` at `dest` (`Alt+F5`).
    ///
    /// `level` picks the encoder: `0` stores, everything else is LZMA2 at that
    /// preset. Written to a temp file, read back, and only then renamed into
    /// place - the "pack then delete sources" may not delete
    /// anything the archive cannot be proved to hold.
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
        let mut writer = sevenz_rust2::ArchiveWriter::new(rewrite.create()?)
            .map_err(|e| Error::msg(format!("{}: {e}", rewrite.path().display())))?;
        writer.set_content_methods(content_methods(level));
        append_puts(&mut writer, plan.puts(), progress)?;
        let mut file = writer.finish().map_err(Error::Bare)?;
        file.flush().map_err(Error::Bare)?;
        drop(file);
        verify_written(self, rewrite.path(), &plan.added_paths(), plan.puts().len())?;
        rewrite.commit()
    }
}

/// p7zip's marker for "this entry is a directory", in the Windows attribute
/// word every 7z carries.
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

/// The encoder chain for a compression level.
///
/// `0` is store; everything else is LZMA2, which is what 7z means by "7z".
/// [`CompressionLevel`]'s `0..=9` is LZMA2's own preset range, so it passes
/// straight through rather than being rescaled.
fn content_methods(level: CompressionLevel) -> Vec<sevenz_rust2::EncoderConfiguration> {
    if level.get() == 0 {
        return vec![sevenz_rust2::EncoderConfiguration::new(
            sevenz_rust2::EncoderMethod::COPY,
        )];
    }
    vec![
        sevenz_rust2::EncoderConfiguration::new(sevenz_rust2::EncoderMethod::LZMA2).with_options(
            sevenz_rust2::encoder_options::EncoderOptions::Lzma2(
                sevenz_rust2::encoder_options::Lzma2Options::from_level(u32::from(level.get())),
            ),
        ),
    ]
}

/// The Windows attribute word that records `mode`, the way p7zip does.
fn attributes_for(mode: u32, is_dir: bool) -> u32 {
    let unix = if is_dir {
        0o40000 | mode
    } else {
        0o100000 | mode
    };
    let mut attributes = (unix << 16) | FILE_ATTRIBUTE_UNIX_EXTENSION;
    if is_dir {
        attributes |= FILE_ATTRIBUTE_DIRECTORY;
    }
    attributes
}

/// Copy one existing entry into the container being built, unless the plan says
/// it does not survive.
///
/// A dropped entry's bytes are still **drained**: 7z hands each member out as a
/// bounded slice of one decoded block, so skipping one without reading it would
/// leave every member after it in that block reading from the wrong offset.
fn transfer_entry<W: Write + Seek>(
    writer: &mut sevenz_rust2::ArchiveWriter<W>,
    entry: &sevenz_rust2::ArchiveEntry,
    body: &mut dyn Read,
    plan: &Plan,
    progress: &mut dyn WriteProgress,
) -> Result<bool> {
    // A name that could escape is not a member as far as the rest of this crate
    // is concerned, and writing it back out would re-arm the payload for
    // whoever unpacks this archive next.
    let Ok(key) = normalize_member(&entry.name, false) else {
        drain(body)?;
        return Ok(false);
    };
    let Fate::Keep(name) = plan.fate(&key) else {
        drain(body)?;
        return Ok(false);
    };
    note(progress, 0)?;

    let mut out = entry.clone();
    out.name = name;
    if out.is_directory || !out.has_stream || out.size == 0 {
        drain(body)?;
        writer
            .push_archive_entry::<&[u8]>(out, None)
            .map_err(|e| Error::msg(format!("{key}: {e}")))?;
        return Ok(true);
    }
    let mut watched = ExactReader::new(WatchedReader::new(body, progress), out.size, key.clone());
    writer
        .push_archive_entry(out, Some(&mut watched))
        .map_err(|e| unwrap_cancellation(Error::msg(format!("{key}: {e}"))))?;
    Ok(true)
}

/// Read and discard what a dropped member holds, to keep the block decoder in
/// step.
fn drain(body: &mut dyn Read) -> Result<()> {
    std::io::copy(body, &mut std::io::sink())
        .map(|_| ())
        .map_err(Error::Bare)
}

/// Write the plan's additions into a 7z being built.
fn append_puts<W: Write + Seek>(
    writer: &mut sevenz_rust2::ArchiveWriter<W>,
    puts: &[Put],
    progress: &mut dyn WriteProgress,
) -> Result<()> {
    for put in puts {
        let mut entry = if put.is_dir() {
            sevenz_rust2::ArchiveEntry::new_directory(&put.member_path)
        } else {
            sevenz_rust2::ArchiveEntry::new_file(&put.member_path)
        };
        entry.has_windows_attributes = true;
        entry.windows_attributes = attributes_for(put.file_mode(), put.is_dir());
        if let Some(when) = put.mtime
            && let Ok(stamp) = sevenz_rust2::NtTime::try_from(when)
        {
            entry.last_modified_date = stamp;
            entry.has_last_modified_date = true;
        }

        let Some(source) = put.source.as_ref() else {
            writer
                .push_archive_entry::<&[u8]>(entry, None)
                .map_err(|e| Error::msg(format!("{}: {e}", put.member_path)))?;
            continue;
        };
        let file = std::fs::File::open(source).map_err(|e| Error::io(source, e))?;
        let size = put.size()?;
        if size == 0 {
            writer
                .push_archive_entry::<&[u8]>(entry, None)
                .map_err(|e| Error::msg(format!("{}: {e}", put.member_path)))?;
            continue;
        }
        // Exactly the size the entry was measured at: a source that shrank
        // under us is an error, never a member the archive under-reports.
        let mut bytes = ExactReader::new(
            WatchedReader::new(file, progress),
            size,
            put.member_path.clone(),
        );
        writer
            .push_archive_entry(entry, Some(&mut bytes))
            .map_err(|e| unwrap_cancellation(Error::msg(format!("{}: {e}", put.member_path))))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::archive::index::{Builder, Index, IndexStatus};
    use std::sync::Arc;

    fn temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hcmd-7z-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn write_7z(path: &Path, members: &[(&str, &[u8])]) {
        let file = std::fs::File::create(path).expect("create");
        let mut writer = sevenz_rust2::ArchiveWriter::new(file).expect("writer");
        for (name, body) in members {
            writer
                .push_archive_entry(
                    sevenz_rust2::ArchiveEntry::new_file(name),
                    Some(std::io::Cursor::new(body.to_vec())),
                )
                .expect("push");
        }
        writer.finish().expect("finish");
    }

    fn index_of(path: &Path) -> Arc<Index> {
        let index = Arc::new(Index::new());
        {
            let mut sink = Builder::new(Arc::clone(&index), false);
            SevenZFormat.index(path, &mut sink).expect("index");
        }
        index.finish(IndexStatus::Complete);
        index
    }

    #[test]
    fn a_member_is_found_by_its_ordinal_and_not_by_its_name() {
        // Two raw names that normalise to the same member path. The index takes
        // the last, as every tool does with a duplicate; a read that matched on
        // the name would hand back the first one's bytes instead.
        let dir = temp("dup");
        let path = dir.join("dup.7z");
        write_7z(&path, &[("dup.txt", b"first"), ("./dup.txt", b"second")]);

        let index = index_of(&path);
        let member = index.get("dup.txt").expect("dup.txt");
        assert_eq!(
            member.locator,
            Locator::Ordinal(1),
            "the later entry is the one the panel shows"
        );
        let mut got = Vec::new();
        SevenZFormat
            .read_member(&path, &member, &mut got)
            .expect("read");
        assert_eq!(got, b"second");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_member_reads_as_nothing_rather_than_as_missing() {
        // A 7z entry with no data is in no compression block at all, so the
        // block walk never sees it; it has to be answered before that.
        let dir = temp("empty");
        let path = dir.join("empty.7z");
        write_7z(&path, &[("nothing.txt", b""), ("something.txt", b"x")]);

        let index = index_of(&path);
        let empty = index.get("nothing.txt").expect("nothing.txt");
        assert_eq!(empty.size, 0);
        let mut got = Vec::new();
        assert_eq!(
            SevenZFormat
                .read_member(&path, &empty, &mut got)
                .expect("an empty member is readable"),
            0
        );
        assert!(got.is_empty());

        let some = index.get("something.txt").expect("something.txt");
        let mut got = Vec::new();
        SevenZFormat
            .read_member(&path, &some, &mut got)
            .expect("read");
        assert_eq!(got, b"x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn members_are_listed_and_read() {
        let dir = temp("read");
        let path = dir.join("a.7z");
        let big = vec![b'7'; 200_000];
        write_7z(&path, &[("d/one.txt", b"hello 7z"), ("d/two.bin", &big)]);

        let index = index_of(&path);
        assert!(index.is_dir("d"), "the parent is synthesised");
        let one = index.get("d/one.txt").expect("one");
        assert_eq!(one.size, 8);

        let mut got = Vec::new();
        SevenZFormat
            .read_member(&path, &one, &mut got)
            .expect("read one");
        assert_eq!(got, b"hello 7z");

        let two = index.get("d/two.bin").expect("two");
        let mut got = Vec::new();
        SevenZFormat
            .read_member(&path, &two, &mut got)
            .expect("read two");
        assert_eq!(got, big);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_escaping_member_is_never_listed() {
        let dir = temp("slip");
        let path = dir.join("evil.7z");
        write_7z(&path, &[("../../etc/passwd", b"x"), ("safe.txt", b"ok")]);
        let index = index_of(&path);
        assert_eq!(index.len(), 1, "the design");
        assert!(index.get("safe.txt").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_is_not_a_7z_is_reported_not_panicked() {
        let dir = temp("bad");
        let path = dir.join("not.7z");
        std::fs::write(&path, b"nope").expect("write");
        let index = Arc::new(Index::new());
        let mut sink = Builder::new(Arc::clone(&index), false);
        assert!(SevenZFormat.index(&path, &mut sink).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A [`WriteProgress`] that counts, and refuses after `stop_after` bytes so
    /// a write can be interrupted the way `Esc` interrupts one.
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

    fn member_bytes(path: &Path, member: &str) -> Vec<u8> {
        let index = index_of(path);
        let found = index
            .get(member)
            .unwrap_or_else(|| panic!("{member} is in the archive"));
        let mut out = Vec::new();
        SevenZFormat
            .read_member(path, &found, &mut out)
            .unwrap_or_else(|e| panic!("read {member}: {e}"));
        out
    }

    #[test]
    fn a_rewrite_adds_removes_and_keeps_the_rest() {
        // the design says `.7z` is writable and the design does not name
        // it; it is classed as a full rewrite because 7z writes its header
        // after its blocks and `sevenz-rust2` has no insertion API.
        let dir = temp("apply");
        let path = dir.join("a.7z");
        let body = vec![b'7'; 120_000];
        write_7z(
            &path,
            &[
                ("keep.txt", b"still here"),
                ("drop/one.txt", b"gone"),
                ("drop/two.txt", b"also gone"),
                ("big.bin", &body),
            ],
        );
        let source = dir.join("new.txt");
        std::fs::write(&source, b"added by the rewrite").expect("write");

        let mut progress = Counter::default();
        SevenZFormat
            .apply(
                &path,
                &[
                    MemberEdit::Remove {
                        member_path: "drop".to_string(),
                    },
                    put("added.txt", &source),
                ],
                &mut progress,
            )
            .expect("apply");

        let index = index_of(&path);
        assert!(
            index.get("drop/one.txt").is_none(),
            "a directory takes what is beneath it"
        );
        assert!(index.get("drop/two.txt").is_none());
        assert_eq!(member_bytes(&path, "keep.txt"), b"still here");
        assert_eq!(member_bytes(&path, "big.bin"), body);
        assert_eq!(member_bytes(&path, "added.txt"), b"added by the rewrite");
        assert!(progress.seen > 0, "the rewrite reported its bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_interrupted_rewrite_never_destroys_the_archive() {
        let dir = temp("cancel");
        let path = dir.join("a.7z");
        write_7z(
            &path,
            &[("keep.txt", b"precious"), ("big.bin", &[5u8; 400_000])],
        );
        let before = std::fs::read(&path).expect("read");

        let source = dir.join("new.txt");
        std::fs::write(&source, b"never lands").expect("write");
        let outcome = SevenZFormat.apply(
            &path,
            &[put("added.txt", &source)],
            &mut Counter {
                seen: 0,
                stop_after: Some(64 * 1024),
            },
        );
        assert!(matches!(outcome, Err(Error::Cancelled)), "{outcome:?}");
        assert_eq!(
            std::fs::read(&path).expect("read"),
            before,
            "the archive is exactly what it was"
        );
        assert_eq!(member_bytes(&path, "keep.txt"), b"precious");
        let names: Vec<String> = std::fs::read_dir(&dir)
            .expect("read_dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "no temp container survives: {names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn packing_writes_a_new_archive_and_stores_at_level_zero() {
        let dir = temp("pack");
        let one = dir.join("one.txt");
        std::fs::write(&one, b"first").expect("write");
        let big = dir.join("big.bin");
        std::fs::write(&big, vec![4u8; 100_000]).expect("write");

        for (tag, level) in [
            ("store", CompressionLevel::STORE),
            ("default", CompressionLevel::DEFAULT),
            ("max", CompressionLevel::MAX),
        ] {
            let dest = dir.join(format!("packed-{tag}.7z"));
            let mut progress = Counter::default();
            SevenZFormat
                .create(
                    &dest,
                    &[
                        MemberEdit::PutDir {
                            member_path: "sub".to_string(),
                            mode: 0o755,
                        },
                        put("sub/one.txt", &one),
                        put("big.bin", &big),
                    ],
                    level,
                    &mut progress,
                )
                .unwrap_or_else(|e| panic!("{tag}: {e}"));
            assert_eq!(member_bytes(&dest, "sub/one.txt"), b"first", "{tag}");
            assert_eq!(member_bytes(&dest, "big.bin"), vec![4u8; 100_000], "{tag}");
            assert!(index_of(&dest).is_dir("sub"), "{tag}");
            assert_eq!(progress.seen, 100_005, "{tag}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cancelled_pack_leaves_nothing_behind() {
        let dir = temp("packcancel");
        let source = dir.join("big.bin");
        std::fs::write(&source, vec![1u8; 400_000]).expect("write");
        let dest = dir.join("packed.7z");
        let outcome = SevenZFormat.create(
            &dest,
            &[put("big.bin", &source)],
            CompressionLevel::DEFAULT,
            &mut Counter {
                seen: 0,
                stop_after: Some(64 * 1024),
            },
        );
        assert!(matches!(outcome, Err(Error::Cancelled)), "{outcome:?}");
        assert!(!dest.exists(), "no half-written archive");
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .expect("read_dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "big.bin")
            .collect();
        assert!(leftovers.is_empty(), "and no temp file: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_written_mode_reads_back_as_the_mode_it_was_given() {
        let dir = temp("mode");
        let source = dir.join("script.sh");
        std::fs::write(&source, b"#!/bin/sh\n").expect("write");
        let dest = dir.join("a.7z");
        SevenZFormat
            .create(
                &dest,
                &[MemberEdit::Put {
                    member_path: "script.sh".to_string(),
                    source: source.clone(),
                    mode: 0o755,
                    mtime: None,
                }],
                CompressionLevel::DEFAULT,
                &mut Counter::default(),
            )
            .expect("create");
        let member = index_of(&dest).get("script.sh").expect("script");
        assert_eq!(member.mode & 0o777, 0o755);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_escaping_member_name_is_refused_on_the_way_in() {
        let dir = temp("slipwrite");
        let path = dir.join("a.7z");
        write_7z(&path, &[("ok.txt", b"ok")]);
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
            let err = SevenZFormat
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
        SevenZFormat
            .apply(
                &path,
                &[put("inside/safe.txt", &source)],
                &mut Counter::default(),
            )
            .expect("a name that stays inside is written");
        assert_eq!(member_bytes(&path, "inside/safe.txt"), b"x");
        assert_eq!(member_bytes(&path, "ok.txt"), b"ok");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
