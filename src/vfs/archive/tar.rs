//! `.tar`, and the four compressed tars.
//!
//! One implementation covers five rows of the table, because the only thing
//! that differs between them is [`Compression`]: `tar` + nothing, `tar` +
//! `flate2`, `tar` + `bzip2`, `tar` + **`liblzma`** (the design rejects
//! `xz2`; see [`super::format`]), `tar` + `zstd`.
//!
//! # A tar has no central directory
//!
//! Which is the whole reason the design says what it says: **listing a
//! compressed tar is reading it**. There is nowhere to seek to, so the index
//! is built by decompressing the stream once, pushing each header into the
//! sink as it is read, and the panel fills as that happens. Nothing is
//! buffered; the memory cost is the index, bounded by
//! [`super::index::MAX_MEMBERS`].
//!
//! Reading a member afterwards costs:
//!
//! * a plain `.tar`: one seek. [`super::index::Locator::Offset`] holds the
//!   file offset the header recorded.
//! * a compressed tar: decompressing from the start of the stream up to that
//!   offset, then the member. There is no cheaper answer that is also correct,
//!   and pretending otherwise is what a seek-emulating cache would be.
//!
//! A **GNU sparse** member is the exception: its bytes on the tape are not its
//! contents, so its locator is an ordinal and reading it walks the entries and
//! lets `tar` reassemble it.

use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};

use super::format::{
    ArchiveFormat, Compression, CompressionLevel, FormatId, MemberEdit, TAR_BLOCK, WriteModel,
    WriteProgress,
};
use super::index::{IndexSink, Locator, Member, MemberKind, RawMember};
use super::rewrite::{
    ExactReader, Fate, Plan, Put, Rewrite, TailGuard, WatchedReader, ensure_room,
    rewrite_footprint, unwrap_cancellation, verify_written,
};
use super::safety::{MAX_MEMBER_PATH, normalize_member};
use super::stream::{copy_exact, skip_exact};

/// The tar backend, parameterised by the compression wrapped around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TarFormat {
    /// Which compression, if any.
    pub compression: Compression,
}

/// `.tar`
pub const TAR: TarFormat = TarFormat {
    compression: Compression::None,
};
/// `.tar.gz` / `.tgz`
pub const TAR_GZ: TarFormat = TarFormat {
    compression: Compression::Gzip,
};
/// `.tar.bz2`
pub const TAR_BZ2: TarFormat = TarFormat {
    compression: Compression::Bzip2,
};
/// `.tar.xz`
pub const TAR_XZ: TarFormat = TarFormat {
    compression: Compression::Xz,
};
/// `.tar.zst`
pub const TAR_ZST: TarFormat = TarFormat {
    compression: Compression::Zstd,
};

impl TarFormat {
    /// The decompressed byte stream of `container`, from its first byte.
    ///
    /// Raw: nothing parses tar headers on this one, so nothing needs guarding.
    /// It is what [`ArchiveFormat::read_member`] skips and copies over.
    fn stream(&self, container: &Path) -> Result<Box<dyn Read + Send>> {
        let file = std::fs::File::open(container).map_err(|e| Error::io(container, e))?;
        self.compression.decoder(file)
    }

    /// The same stream, wrapped in the [`ExtensionGuard`], for the two places
    /// that hand the bytes to `tar`'s own parser.
    fn parsing_stream(&self, container: &Path) -> Result<Box<dyn Read + Send>> {
        Ok(Box::new(ExtensionGuard::new(self.stream(container)?)))
    }
}

/// The most bytes a tar **extension** header may declare before this backend
/// stops believing it.
///
/// `tar` reads a GNU long-name (`L`), a GNU long-link (`K`) and a PAX header
/// (`x`, `g`) **into memory**, because each of them describes the entry that
/// follows and has to be complete before that entry can be built. The size it
/// reads comes out of the container, so a `.tar.gz` of a few kilobytes can name a
/// long-name record of forty gigabytes and cost forty gigabytes of RSS - the
/// "allocate on the strength of a number read out of the file" failure the threat
/// model is about, reached through a library this crate does not own.
///
/// A real long name is bounded by [`super::safety::MAX_MEMBER_PATH`] (4 KiB)
/// and a real PAX record set by a few hundred bytes per key. A megabyte is
/// three orders of magnitude of headroom and still four orders below the point
/// where it matters.
const MAX_EXTENSION_BYTES: u64 = 1024 * 1024;

/// Where a tar header keeps its size, its checksum and its type byte.
const SIZE_FIELD: std::ops::Range<usize> = 124..136;
const CKSUM_FIELD: std::ops::Range<usize> = 148..156;
const TYPE_BYTE: usize = 156;

/// A pass-through reader that refuses a tar whose extension headers are absurd.
///
/// # Why it can be this simple
///
/// Every tar header sits at a 512-byte boundary of the stream, so **every**
/// aligned block is a candidate and there is no layout to track: no PAX `size=`
/// override to follow, no GNU sparse extension blocks to count, and therefore
/// nothing an archive can do to walk this guard out of step with `tar` and hide
/// a header behind the drift.
///
/// It refuses only when all three of these hold at once - the block's checksum
/// is a real tar checksum, its type byte is one of the four extension types,
/// and the size it declares is past [`MAX_EXTENSION_BYTES`]. Anything less is
/// passed through untouched, so a block of file data that happens to be
/// 512-aligned cannot cause a false refusal, and neither can a format detail
/// this guard does not know about. **It fails open**, which is the only safe
/// direction for a check layered over somebody else's parser.
struct ExtensionGuard<R> {
    inner: R,
    block: [u8; TAR_BLOCK],
    have: usize,
    refused: Option<String>,
}

impl<R: Read> ExtensionGuard<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            block: [0u8; TAR_BLOCK],
            have: 0,
            refused: None,
        }
    }
}

impl<R: Read> Read for ExtensionGuard<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if let Some(why) = self.refused.as_ref() {
            return Err(std::io::Error::other(why.clone()));
        }
        let read = self.inner.read(buf)?;
        let mut rest = buf.get(..read).unwrap_or(&[]);
        while !rest.is_empty() {
            let want = TAR_BLOCK.saturating_sub(self.have);
            let take = want.min(rest.len());
            let end = self.have.saturating_add(take);
            if let (Some(into), Some(from)) = (self.block.get_mut(self.have..end), rest.get(..take))
            {
                into.copy_from_slice(from);
            }
            self.have = end;
            rest = rest.get(take..).unwrap_or(&[]);
            if self.have == TAR_BLOCK {
                self.have = 0;
                if let Some(why) = absurd_extension_header(&self.block) {
                    // The bytes just read are still handed back: `tar` gets the
                    // header, asks for its contents, and *that* read is the one
                    // that fails - so the refusal arrives before the allocation
                    // rather than after it.
                    self.refused = Some(why);
                    return Ok(read);
                }
            }
        }
        Ok(read)
    }
}

/// Is this block a tar extension header declaring more than this backend will
/// buffer? The message, if so.
fn absurd_extension_header(block: &[u8; TAR_BLOCK]) -> Option<String> {
    if !checksum_matches(block) {
        return None;
    }
    let kind = *block.get(TYPE_BYTE)?;
    if !matches!(kind, b'L' | b'K' | b'x' | b'g') {
        return None;
    }
    let size = numeric_field(block.get(SIZE_FIELD)?)?;
    if size <= MAX_EXTENSION_BYTES {
        return None;
    }
    Some(format!(
        "a tar '{}' extension header declares {size} bytes of metadata, past the \
         {MAX_EXTENSION_BYTES}-byte limit this reader will hold in memory; the archive is \
 corrupt or hostile",
        char::from(kind)
    ))
}

/// Does this block carry a valid tar header checksum?
///
/// Stricter than [`super::format::looks_like_tar`], which also accepts the
/// `ustar` marker on its own: that is the right question for "might this file
/// be a tar", and this is the right one for "is this block certainly a header",
/// which is what a refusal has to rest on.
fn checksum_matches(block: &[u8; TAR_BLOCK]) -> bool {
    let Some(field) = block.get(CKSUM_FIELD) else {
        return false;
    };
    let Some(claimed) = numeric_field(field) else {
        return false;
    };
    let mut unsigned: u64 = 0;
    let mut signed: i64 = 0;
    for (at, byte) in block.iter().enumerate() {
        let value = if CKSUM_FIELD.contains(&at) {
            b' '
        } else {
            *byte
        };
        unsigned = unsigned.wrapping_add(u64::from(value));
        signed = signed.wrapping_add(i64::from(value as i8));
    }
    // Historical tars computed the sum over signed chars; both answers are
    // accepted, as every tar implementation accepts both.
    claimed == unsigned || i64::try_from(claimed).is_ok_and(|c| c == signed)
}

/// A tar numeric field: NUL/space-terminated octal, or GNU base-256.
///
/// Returns `None` for anything it cannot read as a number, which the callers
/// take as "not a header" rather than as zero.
fn numeric_field(field: &[u8]) -> Option<u64> {
    let first = *field.first()?;
    if first & 0x80 != 0 {
        // GNU base-256. A negative value (`0xff` fill) is not a size or a
        // checksum, and the top bytes above a `u64` must be zero for the rest
        // to mean anything.
        if first != 0x80 {
            return None;
        }
        let rest = field.get(1..)?;
        let split = rest.len().checked_sub(8)?;
        if rest.get(..split)?.iter().any(|b| *b != 0) {
            return None;
        }
        let mut value: u64 = 0;
        for byte in rest.get(split..)? {
            value = value.checked_mul(256)?.checked_add(u64::from(*byte))?;
        }
        return Some(value);
    }
    let text: String = field
        .iter()
        .take_while(|b| **b != 0 && **b != b' ')
        .map(|b| char::from(*b))
        .collect();
    if text.is_empty() {
        return None;
    }
    u64::from_str_radix(&text, 8).ok()
}

/// A tar timestamp, which is seconds since the epoch and may be anything.
fn mtime_of(header: &tar::Header) -> Option<SystemTime> {
    header
        .mtime()
        .ok()
        .and_then(|secs| UNIX_EPOCH.checked_add(Duration::from_secs(secs)))
}

/// Map a tar entry type onto what the panel understands.
///
/// A hard link and a symlink are different things and are kept different: a
/// hard link's target is a member of the same archive, and extracting one
/// wrongly as a symlink would produce a dangling link.
fn kind_of(entry: &tar::Entry<'_, impl Read>) -> MemberKind {
    let header = entry.header();
    let ty = header.entry_type();
    let link = || {
        entry
            .link_name_bytes()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    };
    if ty.is_dir() {
        MemberKind::Dir
    } else if ty.is_symlink() {
        MemberKind::Symlink(link())
    } else if ty.is_hard_link() {
        MemberKind::Hardlink(link())
    } else if ty.is_file() || ty.is_gnu_sparse() {
        MemberKind::File
    } else {
        MemberKind::Other
    }
}

impl ArchiveFormat for TarFormat {
    fn id(&self) -> FormatId {
        match self.compression {
            Compression::None => FormatId::Tar,
            Compression::Gzip => FormatId::TarGz,
            Compression::Bzip2 => FormatId::TarBz2,
            Compression::Xz => FormatId::TarXz,
            Compression::Zstd => FormatId::TarZst,
        }
    }

    fn write_model(&self) -> WriteModel {
        match self.compression {
            // "`.zip` and uncompressed `.tar` support
            // member-level writes and are subject to neither gate."
            Compression::None => WriteModel::Member,
            // "adding one file means decompressing, rewriting and
            // recompressing the whole archive".
            _ => WriteModel::FullRewrite,
        }
    }

    fn release(&self, container: &Path) {
        cursors::forget(container);
    }

    fn index(&self, container: &Path, sink: &mut dyn IndexSink) -> Result<()> {
        let stream = self.parsing_stream(container)?;
        let mut archive = tar::Archive::new(stream);
        let entries = archive.entries().map_err(|e| Error::io(container, e))?;

        for (ordinal, item) in entries.enumerate() {
            if sink.cancelled() {
                return Ok(());
            }
            // A header that does not parse ends the listing: everything after
            // it in the stream is at an unknown offset, so continuing would be
            // guessing. What was read stays listed (the rule about
            // never showing an unreadable thing as an empty one).
            let entry = item.map_err(|e| Error::io(container, e))?;
            let header = entry.header();
            let name = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
            let size = entry.size();
            let sparse = header.entry_type().is_gnu_sparse();
            let kind = kind_of(&entry);
            // A directory header may carry a size, and some pre-POSIX tars did.
            // It is not a size and there are no contents at that offset: what
            // follows a directory header is the next header. Saying so here is
            // what stops `read_member` handing those bytes back as the
            // directory's own.
            let is_dir = matches!(kind, MemberKind::Dir);
            let raw = RawMember {
                name,
                kind,
                size: if is_dir { 0 } else { size },
                mtime: mtime_of(header),
                mode: header.mode().unwrap_or(0),
                uid: u32::try_from(header.uid().unwrap_or(0)).unwrap_or(0),
                gid: u32::try_from(header.gid().unwrap_or(0)).unwrap_or(0),
                locator: if is_dir {
                    Locator::None
                } else if sparse {
                    // The bytes on the tape are not the member's contents.
                    Locator::Ordinal(ordinal)
                } else {
                    Locator::Offset {
                        data: entry.raw_file_position(),
                        len: size,
                    }
                },
            };
            if !sink.push(raw) {
                return Ok(());
            }
        }
        Ok(())
    }

    fn read_member(&self, container: &Path, member: &Member, out: &mut dyn Write) -> Result<u64> {
        match member.locator {
            Locator::Offset { data, len } => match self.compression {
                Compression::None => {
                    // The one place in this module where a seek is possible.
                    let mut file =
                        std::fs::File::open(container).map_err(|e| Error::io(container, e))?;
                    file.seek(SeekFrom::Start(data))
                        .map_err(|e| Error::io(container, e))?;
                    copy_exact(&mut file, out, len)
                }
                _ => {
                    // No central directory, no seek: reaching a member's data
                    // means passing over everything before it.
                    // A decoder already positioned at or before `data` is
                    // resumed rather than replaced - see `cursors`.
                    let key = cursors::Key::of(container);
                    let (mut stream, at) =
                        match key.as_ref().and_then(|key| cursors::take(key, data)) {
                            Some(open) => open,
                            None => (self.stream(container)?, 0),
                        };
                    skip_exact(stream.as_mut(), data.saturating_sub(at))?;
                    let written = copy_exact(stream.as_mut(), out, len)?;
                    // Only a stream whose position is *known* goes back: the
                    // two calls above return early on any failure, so reaching
                    // here means exactly `data + len` bytes have been read.
                    if let Some(key) = key {
                        cursors::put(key, data.saturating_add(len), stream);
                    }
                    Ok(written)
                }
            },
            // A GNU sparse member, or anything else the index could only
            // address by position: walk the entries and let `tar` reassemble
            // it.
            Locator::Ordinal(wanted) => {
                let stream = self.parsing_stream(container)?;
                let mut archive = tar::Archive::new(stream);
                let entries = archive.entries().map_err(|e| Error::io(container, e))?;
                for (ordinal, item) in entries.enumerate() {
                    let mut entry = item.map_err(|e| Error::io(container, e))?;
                    if ordinal != wanted {
                        continue;
                    }
                    let copied = std::io::copy(&mut entry, out).map_err(Error::Bare)?;
                    return Ok(copied);
                }
                Err(Error::NotFound(format!(
                    "{}: entry {wanted} is no longer in {}",
                    member.path,
                    container.display()
                )))
            }
            Locator::None => Err(Error::InvalidPath(format!(
                "{}: a directory has no contents to read",
                member.path
            ))),
        }
    }

    /// Add, replace, remove and rename members.
    ///
    /// Two different operations wear this one name, and which one runs is
    /// decided by the compression:
    ///
    /// * an **uncompressed** `.tar` whose edits are pure additions is appended
    ///   to in place. A tar's end-of-archive marker is two zero blocks, so
    ///   adding a member is: keep those blocks, truncate them off, write the new
    ///   entries, write them back. Adding a file to a 40 GB `.tar` reads the
    ///   headers and writes the new member, and touches nothing else - which is
    ///   what the design means by "`.zip` and uncompressed `.tar` support
    ///   member-level writes and are subject to neither gate".
    /// * everything else is a **full rewrite** into a new container beside the
    ///   original, renamed over it only on success. A compressed
    ///   tar has no other option - "adding one file means decompressing,
    ///   rewriting and recompressing the whole archive" - and an uncompressed
    ///   one needs it too as soon as a member is removed, renamed or replaced,
    ///   because those move every byte after them.
    ///
    /// The size and free-space gates the design puts in front of the second
    /// case belong to the *caller*, before anything is touched
    /// ([`super::session::RewriteLimits`]); what is checked here is only the
    /// room on the filesystem that will actually have to hold both copies.
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
        // Every offset a kept decoder is positioned at is about to become an
        // offset in a container that no longer exists.
        cursors::forget(container);
        if matches!(self.compression, Compression::None) && plan.adds_only() {
            let (end, names) = plain_layout(container)?;
            let taken = plan
                .puts()
                .iter()
                .any(|put| names.iter().any(|name| name == &put.member_path));
            if !taken {
                // A tail too large to hold is the one case that falls through
                // to the rewrite: an in-place append that cannot be undone is
                // not a member-level write, it is a gamble.
                if let Some(guard) = TailGuard::capture(container, end)? {
                    return append_in_place(container, end, guard, plan.puts(), progress);
                }
            }
        }
        self.repack(container, &plan, CompressionLevel::DEFAULT, progress)
    }

    /// Pack `edits` into a brand-new archive at `dest` (`Alt+F5`).
    ///
    /// Written to a temp file and renamed into place, so a pack that fails or
    /// is cancelled part way leaves nothing behind - and **read back before it
    /// is called done**, because the "pack then delete sources" is
    /// only allowed to delete anything once the archive has been proved
    /// readable rather than merely written.
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
        let file = rewrite.create()?;
        self.encode(file, level, |sink| {
            let mut builder = tar::Builder::new(sink);
            append_puts(&mut builder, plan.puts(), progress)?;
            builder.finish().map_err(Error::Bare)
        })?;
        verify_written(self, rewrite.path(), &plan.added_paths(), plan.puts().len())?;
        rewrite.commit()
    }
}

impl TarFormat {
    /// Write a whole new container holding `plan` applied to `container`.
    ///
    /// The one shape that serves every case the in-place append cannot: a
    /// removal, a rename, a replacement, and every edit at all to a compressed
    /// tar. The result is verified and only then renamed over the original, so
    /// a rewrite that produced something unreadable is a *failed* rewrite and
    /// the archive that was there is still there.
    fn repack(
        &self,
        container: &Path,
        plan: &Plan,
        level: CompressionLevel,
        progress: &mut dyn WriteProgress,
    ) -> Result<()> {
        let size = std::fs::metadata(container)
            .map(|meta| meta.len())
            .unwrap_or(0);
        ensure_room(container, rewrite_footprint(size))?;

        let rewrite = Rewrite::beside(container)?;
        let file = rewrite.create()?;
        let mut kept = 0usize;
        self.encode(file, level, |sink| {
            let mut builder = tar::Builder::new(sink);
            let stream = self.parsing_stream(container)?;
            let mut source = tar::Archive::new(stream);
            let entries = source.entries().map_err(|e| Error::io(container, e))?;
            for item in entries {
                let mut entry = item.map_err(|e| Error::io(container, e))?;
                if transfer_entry(&mut builder, &mut entry, plan, progress)? {
                    kept = kept.saturating_add(1);
                }
            }
            append_puts(&mut builder, plan.puts(), progress)?;
            builder.finish().map_err(Error::Bare)
        })?;

        let expected = kept.saturating_add(plan.puts().len());
        verify_written(self, rewrite.path(), &plan.added_paths(), expected)?;
        rewrite.commit()
    }

    /// Run `build` over this format's compression, finishing the encoder
    /// **explicitly**.
    ///
    /// Every encoder here finishes on drop and throws the error away when it
    /// does. For a rewrite that is the difference between a valid archive and a
    /// truncated one that reports success, so the concrete encoder is kept
    /// rather than boxed and its `finish` is checked.
    fn encode<F>(&self, out: std::fs::File, level: CompressionLevel, build: F) -> Result<()>
    where
        F: FnOnce(&mut dyn Write) -> Result<()>,
    {
        fn settle(mut file: std::fs::File) -> Result<()> {
            file.flush().map_err(Error::Bare)?;
            Ok(())
        }
        match self.compression {
            Compression::None => {
                let mut plain = out;
                build(&mut plain)?;
                settle(plain)
            }
            Compression::Gzip => {
                let mut encoder = flate2::write::GzEncoder::new(
                    out,
                    flate2::Compression::new(u32::from(level.get())),
                );
                build(&mut encoder)?;
                settle(encoder.finish().map_err(Error::Bare)?)
            }
            Compression::Bzip2 => {
                // bzip2 has no "store" level; 1 is its floor.
                let mut encoder = bzip2::write::BzEncoder::new(
                    out,
                    bzip2::Compression::new(u32::from(level.get().max(1))),
                );
                build(&mut encoder)?;
                settle(encoder.finish().map_err(Error::Bare)?)
            }
            Compression::Xz => {
                let mut encoder = liblzma::write::XzEncoder::new(out, u32::from(level.get()));
                build(&mut encoder)?;
                settle(encoder.finish().map_err(Error::Bare)?)
            }
            Compression::Zstd => {
                let mut encoder = zstd::stream::write::Encoder::new(out, level.scaled(1, 19))
                    .map_err(Error::Bare)?;
                build(&mut encoder)?;
                settle(encoder.finish().map_err(Error::Bare)?)
            }
        }
    }
}

/// Where an uncompressed tar's entries end, and what is in it.
///
/// Walks the headers and **seeks over the data**, so this costs 512 bytes of
/// reading per member rather than a pass over the container: appending to a
/// 40 GB `.tar` has to be cheap or it is not a member-level write.
///
/// Nothing here allocates from a number read out of the file. A long-name
/// record is read only up to [`super::safety::MAX_MEMBER_PATH`]; every other
/// declared size is seeked over and never held.
///
/// Refuses rather than guesses: a block that is neither zeroes nor a valid
/// header means the rest of the file is at an unknown offset, and appending
/// past that would write a member into the middle of somebody's data.
fn plain_layout(container: &Path) -> Result<(u64, Vec<String>)> {
    let mut file = std::fs::File::open(container).map_err(|e| Error::io(container, e))?;
    let len = file
        .seek(SeekFrom::End(0))
        .map_err(|e| Error::io(container, e))?;
    let block_len = TAR_BLOCK as u64;

    let mut at = 0u64;
    let mut names = Vec::new();
    let mut long_name: Option<String> = None;
    let mut block = [0u8; TAR_BLOCK];

    while at.saturating_add(block_len) <= len {
        file.seek(SeekFrom::Start(at))
            .map_err(|e| Error::io(container, e))?;
        file.read_exact(&mut block)
            .map_err(|e| Error::io(container, e))?;
        if block.iter().all(|byte| *byte == 0) {
            // The end-of-archive marker. Everything past it is padding, and
            // that is where the new members go.
            break;
        }
        if !checksum_matches(&block) {
            return Err(Error::msg(format!(
                "{}: the block at offset {at} is not a tar header, so hcmd cannot tell \
 where this archive ends; it will not append to it",
                container.display()
            )));
        }
        let Some(size) = block.get(SIZE_FIELD).and_then(numeric_field) else {
            return Err(Error::msg(format!(
                "{}: the header at offset {at} declares a size hcmd cannot read",
                container.display()
            )));
        };
        let kind = block.get(TYPE_BYTE).copied().unwrap_or(0);
        let data = at.saturating_add(block_len);
        let padded = size.div_ceil(block_len).saturating_mul(block_len);
        let next = data.saturating_add(padded);
        if next > len || next <= at {
            return Err(Error::msg(format!(
                "{}: the entry at offset {at} declares {size} bytes the archive does not \
                 hold; hcmd will not append to a truncated archive",
                container.display()
            )));
        }
        match kind {
            // A GNU long name belongs to the entry that follows it.
            b'L' => {
                let want = usize::try_from(size.min(MAX_MEMBER_PATH as u64)).unwrap_or(0);
                let mut buf = vec![0u8; want];
                file.seek(SeekFrom::Start(data))
                    .map_err(|e| Error::io(container, e))?;
                file.read_exact(&mut buf)
                    .map_err(|e| Error::io(container, e))?;
                let text = String::from_utf8_lossy(&buf);
                long_name = Some(text.trim_end_matches('\0').to_string());
            }
            // A long link target, a PAX record set: metadata about the next
            // entry, and none of it is a member name of its own.
            b'K' | b'x' | b'g' => {}
            _ => {
                let name = long_name.take().unwrap_or_else(|| ustar_name(&block));
                if let Ok(normalised) = normalize_member(&name, false) {
                    names.push(normalised);
                }
            }
        }
        at = next;
    }
    Ok((at, names))
}

/// The name a plain (non-extended) tar header carries: `prefix` + `name`.
fn ustar_name(block: &[u8; TAR_BLOCK]) -> String {
    let field = |range: std::ops::Range<usize>| -> String {
        block
            .get(range)
            .map(|bytes| {
                let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
                String::from_utf8_lossy(bytes.get(..end).unwrap_or(&[])).into_owned()
            })
            .unwrap_or_default()
    };
    let name = field(0..100);
    // The `ustar` prefix field, which only means anything when the magic is
    // there; a v7 tar has arbitrary bytes at that offset.
    let ustar = block.get(257..262).is_some_and(|m| m == b"ustar");
    let prefix = if ustar {
        field(345..500)
    } else {
        String::new()
    };
    if prefix.is_empty() {
        name
    } else {
        format!("{prefix}/{name}")
    }
}

/// Append members to an uncompressed tar without rewriting it.
///
/// `end` is where the entries stop; `guard` holds the end-of-archive blocks
/// that are about to be overwritten. Every way out of here that is not success
/// puts them back, so an append that fails, or that the user cancels, leaves
/// the archive exactly as it was - and "success" means the
/// headers were walked again afterwards and the new members were found, not
/// merely that the writes returned `Ok`.
fn append_in_place(
    container: &Path,
    end: u64,
    mut guard: TailGuard,
    puts: &[Put],
    progress: &mut dyn WriteProgress,
) -> Result<()> {
    let outcome = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(container)
            .map_err(|e| Error::io(container, e))?;
        file.set_len(end).map_err(|e| Error::io(container, e))?;
        file.seek(SeekFrom::Start(end))
            .map_err(|e| Error::io(container, e))?;
        {
            let mut builder = tar::Builder::new(&mut file);
            append_puts(&mut builder, puts, progress)?;
            builder.finish().map_err(Error::Bare)?;
        }
        file.flush().map_err(|e| Error::io(container, e))?;
        file.sync_all().map_err(|e| Error::io(container, e))?;
        drop(file);

        // Read the headers back before this counts as done. `plain_layout`
        // walks the headers and seeks over the data, so proving a 40 GB tar
        // still parses to its end costs 512 bytes per member and nothing else
        // - which is what makes the check affordable on the very archives it
        // matters most for.
        let (_, names) = plain_layout(container)?;
        for put in puts {
            if !names.iter().any(|name| name == &put.member_path) {
                return Err(Error::msg(format!(
                    "{}: {} was written but cannot be read back out of the archive \
",
                    container.display(),
                    put.member_path
                )));
            }
        }
        Ok(())
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

/// Copy one existing entry into the container being built, unless the plan says
/// it does not survive.
///
/// Returns whether it was written, so a rewrite knows how many members its
/// result must contain.
fn transfer_entry<W: Write>(
    builder: &mut tar::Builder<W>,
    entry: &mut tar::Entry<'_, impl Read>,
    plan: &Plan,
    progress: &mut dyn WriteProgress,
) -> Result<bool> {
    let raw = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
    // A name that could escape is dropped rather than carried across. It is not
    // in the index, so it is not something the user can see or asked to keep,
    // and writing it back out would re-arm the payload for whoever unpacks this
    // archive next.
    let Ok(key) = normalize_member(&raw, false) else {
        return Ok(false);
    };
    let Fate::Keep(name) = plan.fate(&key) else {
        return Ok(false);
    };

    let mut header = entry.header().clone();
    let kind = header.entry_type();
    let size = entry.size();
    // A member the plan did not rename keeps the **bytes** the container
    // stored, not a re-encoding of them.
    //
    // A tar stores a name as bytes with no declared encoding, so `café.txt`
    // written on a Latin-1 system is one byte no decoder can turn into UTF-8.
    // The index decodes lossily - it has to, the panel draws text - and
    // writing that decoding back would replace the byte with U+FFFD *in the
    // archive*, permanently, as a side effect of adding some unrelated file.
    // Round-tripping the original bytes is the only way an edit to one member
    // can be an edit to one member (a rewrite that damages what
    // it was not asked to touch is not a rewrite that succeeded).
    let renamed = name != key;
    let name = out_name(&entry.path_bytes(), &name, renamed, kind.is_dir());

    if kind.is_symlink() || kind.is_hard_link() {
        // The target comes from `entry`, not from the header: a GNU long link
        // target lives in a record `tar` has already consumed, and the header's
        // own field would be the truncated copy.
        let target = entry
            .link_name_bytes()
            .map(|b| b.into_owned())
            .unwrap_or_default();
        header.set_size(0);
        let target = std::path::Path::new(std::ffi::OsStr::from_bytes(&target));
        builder
            .append_link(&mut header, &name, target)
            .map_err(Error::Bare)?;
        return Ok(true);
    }

    if kind.is_gnu_sparse() {
        // The bytes on the tape are not the member's contents; `tar` hands back
        // the reassembled file, so it goes out as an ordinary one. The holes
        // are lost, the contents are not.
        header.set_entry_type(tar::EntryType::Regular);
    }
    header.set_size(size);
    let source = ExactReader::new(
        WatchedReader::new(entry, progress),
        size,
        name.to_string_lossy().into_owned(),
    );
    builder
        .append_data(&mut header, &name, source)
        .map_err(|e| unwrap_cancellation(Error::Bare(e)))?;
    Ok(true)
}

/// Decoders kept open between reads of the same compressed tar.
///
/// # Why this exists
///
/// A compressed tar has no central directory, so [`TarFormat::read_member`]
/// reaches a member by decompressing everything before it. `ops::copy` asks
/// for one member at a time through [`crate::vfs::Vfs::open_read`], which is
/// the only shape the trait has - so extracting an archive of *N*
/// members decompressed the container *N* times, and the cost of `Alt+F6` grew
/// with the square of the member count. Measured on a 300-member `.tar.gz`:
/// one sequential pass 7 ms, the per-member loop 1.14 s.
///
/// # What it does, and what it refuses to do
///
/// A decoder that has just finished serving a member is **kept**, remembering
/// the offset it stopped at. The next read of the same container that wants
/// data at or after that offset resumes it and skips forward instead of
/// starting again, which turns an ascending pass over the members back into
/// one decompression of the container.
///
/// * A stream goes back into the cache **only** when its position is exactly
///   known - after a `copy_exact` that returned `Ok`. A read that failed or was
///   abandoned half way takes its decoder with it, because a decoder at an
///   unknown offset would hand a later caller the wrong bytes.
/// * The key carries the container's size and mtime, so a rewrite
///   (the design renames a whole new file over the old one) can never be
///   read through a decoder built for what was there before.
/// * At most [`MAX_CURSORS`] are held. An xz decoder's window is megabytes;
///   this is a small fixed cost, not a cache to be tuned.
mod cursors {
    use std::io::Read;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::UNIX_EPOCH;

    /// How many open decoders are kept across all containers.
    ///
    /// Two, because the interesting case is one archive being extracted while
    /// another panel reads a second, and because an `xz` decoder's dictionary
    /// is measured in megabytes.
    const MAX_CURSORS: usize = 2;

    /// Which container, and which *version* of it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) struct Key {
        path: std::path::PathBuf,
        len: u64,
        mtime: u64,
    }

    impl Key {
        /// The key for `container`, or `None` when it cannot be measured - in
        /// which case nothing is cached, which is the old behaviour.
        pub(super) fn of(container: &Path) -> Option<Self> {
            let meta = std::fs::metadata(container).ok()?;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            Some(Self {
                path: container.to_path_buf(),
                len: meta.len(),
                mtime,
            })
        }
    }

    struct Cursor {
        key: Key,
        at: u64,
        stream: Box<dyn Read + Send>,
    }

    fn held() -> &'static Mutex<Vec<Cursor>> {
        static HELD: std::sync::OnceLock<Mutex<Vec<Cursor>>> = std::sync::OnceLock::new();
        HELD.get_or_init(|| Mutex::new(Vec::new()))
    }

    fn lock() -> std::sync::MutexGuard<'static, Vec<Cursor>> {
        held().lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A decoder for `key` positioned at or before `wanted`, and where it is.
    pub(super) fn take(key: &Key, wanted: u64) -> Option<(Box<dyn Read + Send>, u64)> {
        let mut held = lock();
        let at = held.iter().position(|c| &c.key == key && c.at <= wanted)?;
        let cursor = held.remove(at);
        Some((cursor.stream, cursor.at))
    }

    /// Keep `stream`, which is positioned at `at` in `key`'s decompressed
    /// bytes.
    pub(super) fn put(key: Key, at: u64, stream: Box<dyn Read + Send>) {
        let mut held = lock();
        // One per container: a second cursor on the same archive would be a
        // second full decompression, which is the thing being avoided.
        held.retain(|c| c.key != key);
        held.push(Cursor { key, at, stream });
        while held.len() > MAX_CURSORS {
            held.remove(0);
        }
    }

    /// Drop every decoder for `container`, whatever version it names.
    ///
    /// Called when this backend has just rewritten the container. The key
    /// would already miss - it carries the size and mtime - but a decoder
    /// holding an open descriptor on an unlinked inode is worth letting go of
    /// at the moment it becomes useless rather than at the next eviction.
    pub(super) fn forget(container: &Path) {
        lock().retain(|c| c.key.path != container);
    }
}

/// The name a transferred entry is written back under.
///
/// `original` is what the container held, byte for byte; `decoded` is the
/// lossily decoded, normalised name the plan speaks in. A member that was
/// **renamed** gets the new name, which is text the user typed and is UTF-8 by
/// construction. Everything else gets its original bytes back. A directory
/// keeps its trailing separator either way, since that is how a tar records
/// that a member is one.
fn out_name(original: &[u8], decoded: &str, renamed: bool, is_dir: bool) -> std::path::PathBuf {
    let mut bytes = if renamed {
        decoded.as_bytes().to_vec()
    } else {
        original.to_vec()
    };
    if is_dir && bytes.last() != Some(&b'/') {
        bytes.push(b'/');
    }
    std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&bytes))
}

/// Write the plan's additions into a tar being built.
fn append_puts<W: Write>(
    builder: &mut tar::Builder<W>,
    puts: &[Put],
    progress: &mut dyn WriteProgress,
) -> Result<()> {
    for put in puts {
        let mut header = tar::Header::new_gnu();
        header.set_mode(put.file_mode());
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(epoch_secs(put.mtime));
        match &put.source {
            None => {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                let name = format!("{}/", put.member_path);
                builder
                    .append_data(&mut header, &name, std::io::empty())
                    .map_err(Error::Bare)?;
            }
            Some(path) => {
                let file = std::fs::File::open(path).map_err(|e| Error::io(path, e))?;
                let size = file.metadata().map_err(|e| Error::io(path, e))?.len();
                header.set_entry_type(tar::EntryType::Regular);
                header.set_size(size);
                let source = ExactReader::new(
                    WatchedReader::new(file, progress),
                    size,
                    put.member_path.clone(),
                );
                builder
                    .append_data(&mut header, &put.member_path, source)
                    .map_err(|e| unwrap_cancellation(Error::Bare(e)))?;
            }
        }
    }
    Ok(())
}

/// Seconds since the epoch, defaulting to now.
fn epoch_secs(when: Option<SystemTime>) -> u64 {
    when.unwrap_or_else(SystemTime::now)
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::archive::index::{Builder, Index, IndexStatus};
    use std::sync::Arc;

    /// Build a tar with the given members, compressed as asked.
    fn write_tar(path: &Path, compression: Compression, members: &[(&str, &[u8])]) {
        let file = std::fs::File::create(path).expect("create");
        let encoder = compression
            .encoder(file, super::super::format::CompressionLevel::new(1))
            .expect("encoder");
        let mut builder = tar::Builder::new(encoder);
        for (name, body) in members {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(1_700_000_000);
            header.set_cksum();
            builder
                .append_data(&mut header, name, std::io::Cursor::new(body))
                .expect("append");
        }
        let mut encoder = builder.into_inner().expect("finish tar");
        encoder.flush().expect("flush");
        drop(encoder);
    }

    fn temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hcmd-tar-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn index_of(format: &TarFormat, path: &Path) -> Arc<Index> {
        let index = Arc::new(Index::new());
        {
            let mut sink = Builder::new(Arc::clone(&index), false);
            format.index(path, &mut sink).expect("index");
        }
        index.finish(IndexStatus::Complete);
        index
    }

    #[test]
    fn every_compression_round_trips() {
        for (tag, compression, format) in [
            ("plain", Compression::None, TAR),
            ("gz", Compression::Gzip, TAR_GZ),
            ("bz2", Compression::Bzip2, TAR_BZ2),
            ("xz", Compression::Xz, TAR_XZ),
            ("zst", Compression::Zstd, TAR_ZST),
        ] {
            let dir = temp(tag);
            let path = dir.join(format!("a.tar.{tag}"));
            let body = vec![b'q'; 200_000];
            write_tar(
                &path,
                compression,
                &[("dir/one.txt", b"hello"), ("dir/two.bin", &body)],
            );

            let index = index_of(&format, &path);
            assert!(index.is_dir("dir"), "{tag}: the parent is synthesised");
            let one = index.get("dir/one.txt").expect("one");
            assert_eq!(one.size, 5, "{tag}");
            assert_eq!(one.mode & 0o777, 0o644, "{tag}");
            assert!(one.mtime.is_some(), "{tag}");

            let mut got = Vec::new();
            format.read_member(&path, &one, &mut got).expect("read one");
            assert_eq!(got, b"hello", "{tag}");

            let two = index.get("dir/two.bin").expect("two");
            let mut got = Vec::new();
            format.read_member(&path, &two, &mut got).expect("read two");
            assert_eq!(got, body, "{tag}: a member larger than a pipe buffer");

            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Write a tar whose member names go into the header **raw**.
    ///
    /// `tar::Builder` refuses to write a `..` path - which is welcome, and is
    /// also why the hostile archive this test needs cannot be built with it.
    /// A real attacker has no such scruples, so the header is filled in by
    /// hand, exactly as one downloaded from the internet would be.
    fn write_tar_raw(path: &Path, members: &[(&str, &[u8])]) {
        let file = std::fs::File::create(path).expect("create");
        let mut builder = tar::Builder::new(file);
        for (name, body) in members {
            let mut header = tar::Header::new_ustar();
            {
                let bytes = header.as_mut_bytes();
                let raw = name.as_bytes();
                bytes
                    .get_mut(..raw.len())
                    .expect("a name shorter than the header")
                    .copy_from_slice(raw);
            }
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(1_700_000_000);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            builder
                .append(&header, std::io::Cursor::new(*body))
                .expect("append");
        }
        builder.finish().expect("finish");
    }

    #[test]
    fn an_escaping_member_is_never_listed() {
        let dir = temp("slip");
        let path = dir.join("evil.tar");
        write_tar_raw(
            &path,
            &[
                ("../../etc/passwd", b"root::0:0"),
                ("/etc/shadow", b"x"),
                ("safe.txt", b"ok"),
            ],
        );
        let index = index_of(&TAR, &path);
        assert!(index.get("safe.txt").is_some());
        assert_eq!(index.len(), 1, "the Zip Slip entries are not in the index");
        assert_eq!(index.refusals().0, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_truncated_archive_lists_what_there_was_and_then_fails() {
        let dir = temp("trunc");
        let path = dir.join("cut.tar.gz");
        write_tar(
            &path,
            Compression::Gzip,
            &[("a.txt", b"aaaa"), ("b.txt", b"bbbb")],
        );
        let full = std::fs::read(&path).expect("read");
        let cut = full.get(..full.len().saturating_sub(40)).unwrap_or(&[]);
        std::fs::write(&path, cut).expect("truncate");

        let index = Arc::new(Index::new());
        let outcome = {
            let mut sink = Builder::new(Arc::clone(&index), false);
            TAR_GZ.index(&path, &mut sink)
        };
        assert!(outcome.is_err(), "a truncated container is reported");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Write a tar whose member names are raw **bytes**, valid UTF-8 or not.
    fn write_tar_bytes(path: &Path, members: &[(&[u8], &[u8])]) {
        let file = std::fs::File::create(path).expect("create");
        let mut builder = tar::Builder::new(file);
        for (name, body) in members {
            let mut header = tar::Header::new_ustar();
            {
                let bytes = header.as_mut_bytes();
                bytes
                    .get_mut(..name.len())
                    .expect("a name shorter than the header")
                    .copy_from_slice(name);
            }
            header.set_size(body.len() as u64);
            header.set_mode(0o600);
            header.set_mtime(1_700_000_000);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            builder
                .append(&header, std::io::Cursor::new(*body))
                .expect("append");
        }
        builder.finish().expect("finish");
    }

    #[test]
    fn a_name_that_is_not_utf8_lists_lossily_and_still_opens() {
        // The same rule `LocalFs` follows: the panel shows U+FFFD, and the
        // member is still readable, because a tar member is addressed by its
        // offset and never by its name.
        let dir = temp("lossy");
        let path = dir.join("latin1.tar");
        write_tar_bytes(&path, &[(b"caf\xe9.txt", b"not utf-8")]);

        let index = index_of(&TAR, &path);
        assert_eq!(index.len(), 1);
        let member = index.get("caf\u{fffd}.txt").expect("the lossy name");
        assert_eq!(member.size, 9);
        let mut got = Vec::new();
        TAR.read_member(&path, &member, &mut got).expect("read");
        assert_eq!(got, b"not utf-8", "a lossy name is still openable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lying_size_is_an_error_and_never_an_allocation() {
        // The header claims four gigabytes; the container holds nine bytes.
        // Nothing may be allocated on the strength of that number, and the
        // short stream is an error rather than a silent truncation.
        let dir = temp("lies");
        let path = dir.join("liar.tar");
        write_tar_bytes(&path, &[(b"small.txt", b"nine byte")]);
        let mut raw = std::fs::read(&path).expect("read");
        {
            let mut header = tar::Header::new_ustar();
            header
                .as_mut_bytes()
                .get_mut(..9)
                .expect("room")
                .copy_from_slice(b"small.txt");
            header.set_size(4 * 1024 * 1024 * 1024);
            header.set_mode(0o600);
            header.set_mtime(1_700_000_000);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            raw.get_mut(..TAR_BLOCK)
                .expect("the first block")
                .copy_from_slice(header.as_bytes());
        }
        std::fs::write(&path, &raw).expect("rewrite");

        let index = Arc::new(Index::new());
        {
            let mut sink = Builder::new(Arc::clone(&index), false);
            let _ = TAR.index(&path, &mut sink);
        }
        index.finish(IndexStatus::Complete);
        let member = index.get("small.txt").expect("the member is listed");
        assert_eq!(member.size, 4 * 1024 * 1024 * 1024, "the claim is recorded");
        let mut got = Vec::new();
        let err = TAR
            .read_member(&path, &member, &mut got)
            .expect_err("a claim the container cannot honour is an error");
        assert!(err.to_string().contains("before the entry"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_absurd_extension_header_is_refused_before_it_is_buffered() {
        // `tar` reads a GNU long-name record into memory whole, and the length
        // comes out of the container. A gzipped kilobyte that declares four
        // gigabytes of it would otherwise cost four gigabytes of RSS.
        let dir = temp("bomb");
        let path = dir.join("namebomb.tar.gz");
        let mut header = tar::Header::new_gnu();
        header.set_size(4 * 1024 * 1024 * 1024);
        header.set_entry_type(tar::EntryType::GNULongName);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        assert!(
            absurd_extension_header(header.as_bytes()).is_some(),
            "the guard recognises the header"
        );

        let file = std::fs::File::create(&path).expect("create");
        let mut encoder = Compression::Gzip
            .encoder(file, super::super::format::CompressionLevel::new(1))
            .expect("encoder");
        encoder.write_all(header.as_bytes()).expect("header");
        encoder.write_all(&[b'A'; 4096]).expect("some of the name");
        encoder.flush().expect("flush");
        drop(encoder);

        let index = Arc::new(Index::new());
        let outcome = {
            let mut sink = Builder::new(Arc::clone(&index), false);
            TAR_GZ.index(&path, &mut sink)
        };
        let err = outcome.expect_err("the archive is refused");
        assert!(err.to_string().contains("extension header"), "{err}");
        assert!(index.is_empty(), "nothing was listed from it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_real_long_name_still_lists() {
        // The other half of the guard: a legitimate GNU long name - anything
        // past the 100 bytes a ustar header holds - must pass through
        // untouched, or the guard would be worse than the bug.
        let dir = temp("longname");
        let path = dir.join("long.tar.gz");
        let long = format!("{}/{}.txt", "d".repeat(90), "n".repeat(120));
        write_tar(&path, Compression::Gzip, &[(long.as_str(), b"hello")]);

        let index = index_of(&TAR_GZ, &path);
        let member = index.get(&long).expect("the long name survives");
        let mut got = Vec::new();
        TAR_GZ.read_member(&path, &member, &mut got).expect("read");
        assert_eq!(got, b"hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_guard_only_fires_on_a_real_extension_header() {
        // Fail-open is the whole design: anything the guard cannot positively
        // identify is passed through rather than refused.
        let mut ordinary = tar::Header::new_gnu();
        ordinary.set_size(40 * 1024 * 1024 * 1024);
        ordinary.set_entry_type(tar::EntryType::Regular);
        ordinary.set_cksum();
        assert!(
            absurd_extension_header(ordinary.as_bytes()).is_none(),
            "a huge *file* is a normal archive, not an attack on memory"
        );

        let mut small = tar::Header::new_gnu();
        small.set_size(200);
        small.set_entry_type(tar::EntryType::GNULongName);
        small.set_cksum();
        assert!(absurd_extension_header(small.as_bytes()).is_none());

        let mut broken = tar::Header::new_gnu();
        broken.set_size(4 * 1024 * 1024 * 1024);
        broken.set_entry_type(tar::EntryType::GNULongName);
        broken.set_cksum();
        let mut bytes = *broken.as_bytes();
        bytes[0] ^= 0xff; // now the checksum does not match
        assert!(
            absurd_extension_header(&bytes).is_none(),
            "a block that is not certainly a header is not refused"
        );

        assert!(absurd_extension_header(&[0u8; TAR_BLOCK]).is_none());
        assert!(absurd_extension_header(&[b'A'; TAR_BLOCK]).is_none());
    }

    #[test]
    fn numeric_fields_read_octal_and_base_256_or_decline() {
        assert_eq!(numeric_field(b"0000144\0"), Some(0o144));
        assert_eq!(numeric_field(b"0000144 "), Some(0o144));
        assert_eq!(numeric_field(b"        "), None, "an empty field");
        assert_eq!(numeric_field(b"99999999"), None, "9 is not an octal digit");
        assert_eq!(numeric_field(&[]), None);
        // GNU base-256: 0x80, then eleven bytes big-endian.
        let mut field = [0u8; 12];
        field[0] = 0x80;
        field[11] = 7;
        assert_eq!(numeric_field(&field), Some(7));
        field[0] = 0xff; // negative
        assert_eq!(numeric_field(&field), None);
    }

    #[test]
    fn the_write_model_follows_the_compression() {
        assert_eq!(TAR.write_model(), WriteModel::Member);
        for format in [TAR_GZ, TAR_BZ2, TAR_XZ, TAR_ZST] {
            assert_eq!(format.write_model(), WriteModel::FullRewrite);
            assert!(format.capabilities().writable);
        }
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
            mtime: Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
        }
    }

    fn member_bytes(format: &TarFormat, path: &Path, member: &str) -> Vec<u8> {
        let index = index_of(format, path);
        let found = index
            .get(member)
            .unwrap_or_else(|| panic!("{member} is in the archive"));
        let mut out = Vec::new();
        format
            .read_member(path, &found, &mut out)
            .unwrap_or_else(|e| panic!("read {member}: {e}"));
        out
    }

    #[test]
    fn appending_to_a_plain_tar_does_not_rewrite_it() {
        // "`.zip` and uncompressed `.tar` support member-level
        // writes." The proof is that every byte of the members that were
        // already there is where it was: only the end-of-archive blocks moved.
        let dir = temp("append");
        let path = dir.join("a.tar");
        write_tar(
            &path,
            Compression::None,
            &[("keep.txt", b"keep me"), ("d/deep.bin", &[7u8; 5000])],
        );
        let before = std::fs::read(&path).expect("read");
        let (end, names) = plain_layout(&path).expect("layout");
        assert_eq!(names, ["keep.txt", "d/deep.bin"], "the headers were walked");
        let end = usize::try_from(end).expect("end fits");

        let source = dir.join("new.txt");
        std::fs::write(&source, b"a brand new member").expect("write");
        let mut progress = Counter::default();
        TAR.apply(&path, &[put("added/new.txt", &source)], &mut progress)
            .expect("apply");

        let after = std::fs::read(&path).expect("read");
        assert_eq!(
            after.get(..end),
            before.get(..end),
            "the members that were there were not moved"
        );
        assert_eq!(member_bytes(&TAR, &path, "keep.txt"), b"keep me");
        assert_eq!(member_bytes(&TAR, &path, "d/deep.bin"), vec![7u8; 5000]);
        assert_eq!(
            member_bytes(&TAR, &path, "added/new.txt"),
            b"a brand new member"
        );
        assert_eq!(progress.seen, 18, "every source byte was reported");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_interrupted_append_leaves_the_tar_byte_for_byte() {
        let dir = temp("cancel-append");
        let path = dir.join("a.tar");
        write_tar(&path, Compression::None, &[("keep.txt", b"keep me")]);
        let before = std::fs::read(&path).expect("read");

        let source = dir.join("big.bin");
        std::fs::write(&source, vec![3u8; 400_000]).expect("write");
        let outcome = TAR.apply(
            &path,
            &[put("big.bin", &source)],
            &mut Counter {
                seen: 0,
                stop_after: Some(64 * 1024),
            },
        );
        assert!(matches!(outcome, Err(Error::Cancelled)), "{outcome:?}");
        assert_eq!(
            std::fs::read(&path).expect("read"),
            before,
            "an interrupted member-level write restores the end-of-archive blocks"
        );
        assert_eq!(member_bytes(&TAR, &path, "keep.txt"), b"keep me");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_compression_rewrites_and_round_trips() {
        for (tag, compression, format) in [
            ("plain", Compression::None, TAR),
            ("gz", Compression::Gzip, TAR_GZ),
            ("bz2", Compression::Bzip2, TAR_BZ2),
            ("xz", Compression::Xz, TAR_XZ),
            ("zst", Compression::Zstd, TAR_ZST),
        ] {
            let dir = temp(&format!("rewrite-{tag}"));
            let path = dir.join(format!("a.tar.{tag}"));
            let body = vec![b'q'; 120_000];
            write_tar(
                &path,
                compression,
                &[
                    ("keep.txt", b"still here"),
                    ("drop/one.txt", b"gone"),
                    ("drop/two.txt", b"also gone"),
                    ("big.bin", &body),
                ],
            );
            let source = dir.join("new.txt");
            std::fs::write(&source, b"added by the rewrite").expect("write");

            format
                .apply(
                    &path,
                    &[
                        MemberEdit::Remove {
                            member_path: "drop".to_string(),
                        },
                        put("added.txt", &source),
                    ],
                    &mut Counter::default(),
                )
                .unwrap_or_else(|e| panic!("{tag}: {e}"));

            let index = index_of(&format, &path);
            assert!(index.get("drop/one.txt").is_none(), "{tag}");
            assert!(index.get("drop/two.txt").is_none(), "{tag}");
            assert_eq!(
                member_bytes(&format, &path, "keep.txt"),
                b"still here",
                "{tag}"
            );
            assert_eq!(member_bytes(&format, &path, "big.bin"), body, "{tag}");
            assert_eq!(
                member_bytes(&format, &path, "added.txt"),
                b"added by the rewrite",
                "{tag}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn an_interrupted_rewrite_never_destroys_the_archive() {
        // in as many words: "an interrupted or failed rewrite
        // never destroys the archive. The original is unlinked only after the
        // rename succeeds." So: interrupt one.
        let dir = temp("cancel-rewrite");
        let path = dir.join("a.tar.gz");
        write_tar(
            &path,
            Compression::Gzip,
            &[("keep.txt", b"precious"), ("big.bin", &[5u8; 300_000])],
        );
        let before = std::fs::read(&path).expect("read");

        let source = dir.join("new.txt");
        std::fs::write(&source, b"never lands").expect("write");
        let outcome = TAR_GZ.apply(
            &path,
            &[put("added.txt", &source)],
            &mut Counter {
                seen: 0,
                stop_after: Some(64 * 1024),
            },
        );
        assert!(
            matches!(outcome, Err(Error::Cancelled)),
            "a cancelled rewrite is cancelled, not failed: {outcome:?}"
        );
        assert_eq!(
            std::fs::read(&path).expect("read"),
            before,
            "the archive is exactly what it was"
        );
        assert_eq!(member_bytes(&TAR_GZ, &path, "keep.txt"), b"precious");
        let names: Vec<String> = std::fs::read_dir(&dir)
            .expect("read_dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "no temp container survives: {names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rename_moves_a_directory_and_everything_in_it() {
        let dir = temp("rename");
        let path = dir.join("a.tar");
        write_tar(
            &path,
            Compression::None,
            &[("old/one.txt", b"1"), ("old/two/three.txt", b"3")],
        );
        TAR.apply(
            &path,
            &[MemberEdit::Rename {
                from: "old".to_string(),
                to: "new".to_string(),
            }],
            &mut Counter::default(),
        )
        .expect("apply");
        assert_eq!(member_bytes(&TAR, &path, "new/one.txt"), b"1");
        assert_eq!(member_bytes(&TAR, &path, "new/two/three.txt"), b"3");
        assert!(index_of(&TAR, &path).get("old/one.txt").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_symlink_and_a_directory_survive_a_rewrite_as_themselves() {
        let dir = temp("kinds");
        let path = dir.join("a.tar");
        {
            let file = std::fs::File::create(&path).expect("create");
            let mut builder = tar::Builder::new(file);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o750);
            header.set_size(0);
            header.set_mtime(1_700_000_000);
            builder
                .append_data(&mut header, "d/", std::io::empty())
                .expect("dir");
            let mut link = tar::Header::new_gnu();
            link.set_entry_type(tar::EntryType::Symlink);
            link.set_mode(0o777);
            link.set_size(0);
            link.set_mtime(1_700_000_000);
            builder
                .append_link(&mut link, "d/link", "target.txt")
                .expect("link");
            let mut reg = tar::Header::new_gnu();
            reg.set_size(4);
            reg.set_mode(0o644);
            reg.set_mtime(1_700_000_000);
            builder
                .append_data(&mut reg, "gone.txt", std::io::Cursor::new(b"gone"))
                .expect("file");
            builder.finish().expect("finish");
        }

        TAR.apply(
            &path,
            &[MemberEdit::Remove {
                member_path: "gone.txt".to_string(),
            }],
            &mut Counter::default(),
        )
        .expect("apply");

        let index = index_of(&TAR, &path);
        let link = index.get("d/link").expect("the link survives");
        assert_eq!(link.kind, MemberKind::Symlink("target.txt".to_string()));
        let d = index.get("d").expect("the directory survives");
        assert_eq!(d.kind, MemberKind::Dir);
        assert_eq!(d.mode & 0o777, 0o750, "and keeps its mode");
        assert!(index.get("gone.txt").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn packing_writes_a_new_archive_at_every_compression() {
        for (tag, format) in [
            ("tar", TAR),
            ("tar.gz", TAR_GZ),
            ("tar.bz2", TAR_BZ2),
            ("tar.xz", TAR_XZ),
            ("tar.zst", TAR_ZST),
        ] {
            let dir = temp(&format!("pack-{tag}"));
            let one = dir.join("one.txt");
            std::fs::write(&one, b"first").expect("write");
            let big = dir.join("big.bin");
            std::fs::write(&big, vec![2u8; 100_000]).expect("write");
            let dest = dir.join(format!("packed.{tag}"));

            let mut progress = Counter::default();
            format
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
                    CompressionLevel::DEFAULT,
                    &mut progress,
                )
                .unwrap_or_else(|e| panic!("{tag}: {e}"));

            assert_eq!(
                member_bytes(&format, &dest, "sub/one.txt"),
                b"first",
                "{tag}"
            );
            assert_eq!(
                member_bytes(&format, &dest, "big.bin"),
                vec![2u8; 100_000],
                "{tag}"
            );
            assert!(index_of(&format, &dest).is_dir("sub"), "{tag}");
            assert_eq!(progress.seen, 100_005, "{tag}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_cancelled_pack_leaves_nothing_behind() {
        let dir = temp("packcancel");
        let source = dir.join("big.bin");
        std::fs::write(&source, vec![1u8; 400_000]).expect("write");
        let dest = dir.join("packed.tar.zst");
        let outcome = TAR_ZST.create(
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
    fn an_escaping_member_name_is_refused_on_the_way_in() {
        let dir = temp("slipwrite");
        let path = dir.join("a.tar");
        write_tar(&path, Compression::None, &[("ok.txt", b"ok")]);
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
            let err = TAR
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
        TAR.apply(
            &path,
            &[put("inside/safe.txt", &source)],
            &mut Counter::default(),
        )
        .expect("a name that stays inside is written");
        assert_eq!(member_bytes(&TAR, &path, "inside/safe.txt"), b"x");
        assert_eq!(member_bytes(&TAR, &path, "ok.txt"), b"ok");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_escaping_member_is_not_carried_across_a_rewrite() {
        // The index refuses it, so the user cannot see it or ask for it; the
        // rewrite drops it rather than writing the payload back out for
        // whoever unpacks the archive next.
        let dir = temp("sliprewrite");
        let path = dir.join("evil.tar");
        write_tar_raw(
            &path,
            &[("../../etc/passwd", b"root::0:0"), ("safe.txt", b"ok")],
        );
        let source = dir.join("src.txt");
        std::fs::write(&source, b"new").expect("write");
        TAR.apply(
            &path,
            &[
                MemberEdit::Remove {
                    member_path: "safe.txt".to_string(),
                },
                put("fresh.txt", &source),
            ],
            &mut Counter::default(),
        )
        .expect("apply");

        let index = index_of(&TAR, &path);
        assert_eq!(index.refusals().0, 0, "the hostile entry is simply gone");
        assert_eq!(index.len(), 1);
        assert_eq!(member_bytes(&TAR, &path, "fresh.txt"), b"new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn appending_to_a_tar_hcmd_cannot_read_to_the_end_is_refused() {
        // Where the entries end is what an append needs to know; a container
        // whose headers stop making sense cannot answer it, and guessing would
        // write a member into the middle of somebody's data.
        let dir = temp("garbage");
        let path = dir.join("a.tar");
        write_tar(&path, Compression::None, &[("a.txt", b"aaaa")]);
        let mut bytes = std::fs::read(&path).expect("read");
        // Overwrite the end-of-archive blocks with something that is neither
        // zeroes nor a header.
        let at = 1024;
        for (i, byte) in b"not a tar header at all".iter().enumerate() {
            if let Some(slot) = bytes.get_mut(at + i) {
                *slot = *byte;
            }
        }
        std::fs::write(&path, &bytes).expect("write");
        assert!(plain_layout(&path).is_err(), "refused, not guessed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
