//! The format layer.
//!
//! Eight formats, one trait. A format is a value implementing
//! [`ArchiveFormat`], not a match arm repeated in six places: listing,
//! reading, writing and deleting each dispatch once, here, and adding a ninth
//! format means adding one file and one row to [`FORMATS`].
//!
//! # The table, and one correction
//!
//! the design gives the backends as `zip`, `tar`, `tar`+`flate2`,
//! `tar`+`bzip2`, `tar`+**`xz2`**, `tar`+`zstd`, `sevenz-rust2` and `unrar`.
//! the design **rejects `xz2`** - last release 2022-06-06 - and names
//! `liblzma`, "which is maintained and API-compatible". The two sections
//! contradict each other; the rejected-crates table is the authority, so
//! `.tar.xz` is `liblzma` here. Recorded in the design.
//!
//! Everything is an in-process Rust crate. Nothing shells out: no `bsdtar`, no
//! `7z`, no `unzip`, and no `compress-tools`, which the design rejects for
//! wrapping libarchive.
//!
//! # Detection is by content, then by extension
//!
//! "Detection is by content sniffing first, extension second -
//! TC users routinely have archives with wrong extensions." [`detect`] reads
//! the first [`SNIFF_LEN`] bytes and matches [`MAGIC`]; only when the content
//! says nothing does it fall back to the name. `infer` is not among this
//! milestone's dependencies and the table for eight formats is short enough to
//! own outright.
//!
//! A compressed stream needs a second question: gzip magic says "gzip", not
//! "tar inside gzip". [`detect`] decompresses the first 512 bytes and checks
//! for a tar header, so a `.tar.gz` is recognised whatever it is called and a
//! bare `foo.gz` is reported as unsupported rather than opened as an empty
//! directory.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::{Error, Result};
use crate::vfs::{Capabilities, LatencyClass};

use super::index::{IndexSink, Member};

/// How many bytes [`detect`] reads to sniff a container.
///
/// 512 covers every magic in [`MAGIC`] and the `ustar` marker a tar carries at
/// offset 257.
pub const SNIFF_LEN: usize = 512;

/// One tar header block.
pub const TAR_BLOCK: usize = 512;

/// The eight formats of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FormatId {
    /// `.zip`
    Zip,
    /// `.tar`
    Tar,
    /// `.tar.gz` / `.tgz`
    TarGz,
    /// `.tar.bz2`
    TarBz2,
    /// `.tar.xz`
    TarXz,
    /// `.tar.zst`
    TarZst,
    /// `.7z`
    SevenZ,
    /// `.rar` - read only. RAR compression is patent-encumbered and `unrar`
    /// cannot write.
    Rar,
    /// A `.gz` that is not a tar: one compressed file, read only.
    Gz,
    /// A `.bz2` that is not a tar.
    Bz2,
    /// A `.xz` that is not a tar - a compressed disk image, most often.
    Xz,
    /// A `.zst` that is not a tar.
    Zst,
}

impl FormatId {
    /// Every format, in the order the table lists them.
    pub const ALL: &'static [Self] = &[
        Self::Zip,
        Self::Tar,
        Self::TarGz,
        Self::TarBz2,
        Self::TarXz,
        Self::TarZst,
        Self::SevenZ,
        Self::Rar,
        Self::Gz,
        Self::Bz2,
        Self::Xz,
        Self::Zst,
    ];

    /// The name shown in the pack dialog and in error messages.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::Tar => "tar",
            Self::TarGz => "tar.gz",
            Self::TarBz2 => "tar.bz2",
            Self::TarXz => "tar.xz",
            Self::TarZst => "tar.zst",
            Self::SevenZ => "7z",
            Self::Rar => "rar",
            Self::Gz => "gz",
            Self::Bz2 => "bz2",
            Self::Xz => "xz",
            Self::Zst => "zst",
        }
    }

    /// The extension `Alt+F5` gives a new archive of this format, **with its
    /// leading dot**.
    ///
    /// The dot is part of it because every use of this appends to a stem, and
    /// an extension without one produces `photoszip`. It is also what makes
    /// this round-trip through [`FormatId::from_name`], which matches suffixes
    /// that include the dot so that a file merely *called* `zip` is not read as
    /// an archive.
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Zip => ".zip",
            Self::Tar => ".tar",
            Self::TarGz => ".tar.gz",
            Self::TarBz2 => ".tar.bz2",
            Self::TarXz => ".tar.xz",
            Self::TarZst => ".tar.zst",
            Self::SevenZ => ".7z",
            Self::Rar => ".rar",
            Self::Gz => ".gz",
            Self::Bz2 => ".bz2",
            Self::Xz => ".xz",
            Self::Zst => ".zst",
        }
    }

    /// Every suffix that names a format, longest first.
    ///
    /// Longest first is not a tidiness: `.tar.gz` read as a `.gz` is a
    /// singly-compressed file rather than an archive, and `.tar.zst` read as a
    /// `.zst` is the same mistake.
    const BY_SUFFIX: &'static [(&'static str, Self)] = &[
        (".tar.zstd", Self::TarZst),
        (".tar.bz2", Self::TarBz2),
        (".tar.zst", Self::TarZst),
        (".tar.gz", Self::TarGz),
        (".tar.xz", Self::TarXz),
        (".tbz2", Self::TarBz2),
        (".tzst", Self::TarZst),
        (".tgz", Self::TarGz),
        (".tbz", Self::TarBz2),
        (".txz", Self::TarXz),
        (".zip", Self::Zip),
        (".tar", Self::Tar),
        (".rar", Self::Rar),
        (".7z", Self::SevenZ),
        // Last, and after every `.tar.*` and `.t*z` row above: a bare
        // compression suffix is a singly compressed file, and reading
        // `.tar.gz` as one of those is the mistake the ordering prevents.
        (".zst", Self::Zst),
        (".bz2", Self::Bz2),
        (".gz", Self::Gz),
        (".xz", Self::Xz),
    ];

    /// The suffix of `name` that names a format, and which format that is.
    ///
    /// The suffix itself and not merely the format, because a caller replacing
    /// one extension with another has to remove exactly what is there:
    /// `photos.tgz` and `photos.tar.gz` are the same format and four characters
    /// apart, and stripping [`FormatId::extension`]'s length from the first
    /// eats part of the name.
    pub fn suffix_of(name: &str) -> Option<(&'static str, Self)> {
        let lower = name.to_ascii_lowercase();
        Self::BY_SUFFIX
            .iter()
            .find(|(suffix, _)| lower.ends_with(suffix))
            .copied()
    }

    /// The format a *name* suggests. The second question, never the first.
    ///
    ///
    /// Case-insensitive, longest suffix first, so `.tar.gz` is not read as a
    /// `.gz` and `.tar.zst` is not read as a `.zst`.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::suffix_of(name).map(|(_, id)| id)
    }

    /// The implementation.
    pub fn backend(self) -> &'static dyn ArchiveFormat {
        match self {
            Self::Zip => &super::zip::ZipFormat,
            Self::Tar => &super::tar::TAR,
            Self::TarGz => &super::tar::TAR_GZ,
            Self::TarBz2 => &super::tar::TAR_BZ2,
            Self::TarXz => &super::tar::TAR_XZ,
            Self::TarZst => &super::tar::TAR_ZST,
            Self::SevenZ => &super::sevenz::SevenZFormat,
            Self::Rar => &super::rar::RarFormat,
            Self::Gz => &super::single::GZ,
            Self::Bz2 => &super::single::BZ2,
            Self::Xz => &super::single::XZ,
            Self::Zst => &super::single::ZST,
        }
    }
}

impl std::fmt::Display for FormatId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// How a format accepts a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteModel {
    /// Never. `.rar`: reading only, so `F5` *into* one is refused up front
    /// with a clear message rather than failing halfway.
    None,
    /// A member can be added, replaced or removed in place. `.zip` and an
    /// uncompressed `.tar`, which the design exempts from both gates.
    Member,
    /// Adding one file means rewriting the whole container: every compressed
    /// tar. Gated on size and free space by the design before anything is
    /// touched.
    FullRewrite,
}

impl WriteModel {
    /// Can this format be written at all?
    pub const fn writable(self) -> bool {
        !matches!(self, Self::None)
    }

    /// Does a write have to pass the gates?
    pub const fn needs_rewrite_gates(self) -> bool {
        matches!(self, Self::FullRewrite)
    }
}

/// How a member's bytes are obtained (the "streaming where the
/// format allows").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberSource {
    /// The library can be driven to produce the member's bytes into a
    /// [`Write`], so a read is a stream and costs no disk.
    Stream,
    /// The library will only extract to a file. `.rar`: `unrar` offers
    /// "into a `Vec<u8>`" or "into a path", and the first is the whole member
    /// in memory. Reads therefore go through the session cache, which is
    /// bounded by disk rather than by RAM.
    Materialise,
}

/// One change to an archive, for [`ArchiveFormat::apply`] and
/// [`ArchiveFormat::create`].
///
/// Deliberately expressed over **local files**, not over readers: every write
/// this milestone performs starts from a file the user selected (`F5`,
/// `Alt+F5`) or from a temp file the editor round trip produced, and a
/// compressed-tar rewrite has to be able to re-read a member it has already
/// streamed once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberEdit {
    /// Add `member_path`, replacing any member already there, with the
    /// contents of the local file `source`.
    Put {
        /// The normalised member path.
        member_path: String,
        /// The local file holding the bytes.
        source: std::path::PathBuf,
        /// Mode bits to record, `0` for the format's default.
        mode: u32,
        /// Modification time to record.
        mtime: Option<std::time::SystemTime>,
    },
    /// Add a directory member.
    PutDir {
        /// The normalised member path.
        member_path: String,
        /// Mode bits to record, `0` for the format's default.
        mode: u32,
    },
    /// Remove a member; a directory takes everything beneath it.
    Remove {
        /// The normalised member path.
        member_path: String,
    },
    /// Rename a member; a directory takes everything beneath it. Formats with
    /// no rename of their own implement it as a remove and an add.
    Rename {
        /// The normalised member path now.
        from: String,
        /// The normalised member path after.
        to: String,
    },
}

impl MemberEdit {
    /// The member this edit is about - the destination, for a rename.
    pub fn member_path(&self) -> &str {
        match self {
            Self::Put { member_path, .. }
            | Self::PutDir { member_path, .. }
            | Self::Remove { member_path } => member_path,
            Self::Rename { to, .. } => to,
        }
    }
}

/// Compression effort for `Alt+F5`.
///
/// `0` is store, `9` is maximum. Every format maps this onto its own scale;
/// none of them are asked to interpret a number outside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CompressionLevel(u8);

impl CompressionLevel {
    /// Store, no compression.
    pub const STORE: Self = Self(0);
    /// The default the pack dialog opens on.
    pub const DEFAULT: Self = Self(6);
    /// Maximum.
    pub const MAX: Self = Self(9);

    /// Clamp anything into `0..=9`.
    pub const fn new(level: u8) -> Self {
        Self(if level > 9 { 9 } else { level })
    }

    /// The level as a number in `0..=9`.
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Rescale into `[min, max]`, for a format with a different range (zstd
    /// goes to 22, xz to 9, and 7z's presets are its own).
    pub fn scaled(self, min: i32, max: i32) -> i32 {
        let span = max.saturating_sub(min);
        min.saturating_add((i32::from(self.0).saturating_mul(span)).saturating_div(9))
    }
}

impl Default for CompressionLevel {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Where a write reports its progress.
///
/// the design settles that the transfer rate is measured once, in the copy
/// loop above the [`crate::vfs::Vfs`] trait, so reads carry no progress at
/// all. A **rewrite** is the exception that proves it: the bytes never pass
/// through any copy loop - the format is copying members from one container to
/// another - so the only place that can count them is inside the format.
pub trait WriteProgress: Send {
    /// `n` more bytes have been written. Return `false` to cancel; the format
    /// must then abandon the rewrite without touching the original.
    fn bytes(&mut self, n: u64) -> bool;
}

/// A [`WriteProgress`] that counts nothing and never cancels.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoProgress;

impl WriteProgress for NoProgress {
    fn bytes(&mut self, _n: u64) -> bool {
        true
    }
}

/// One archive format.
///
/// Implementations are zero-sized and `'static`: a format is a set of
/// functions, not a state machine, and everything stateful - the index, the
/// temp files, the caches - belongs to [`super::ArchiveFs`] and
/// [`super::ArchiveSession`], where it is shared between formats rather than
/// reimplemented by each of them.
pub trait ArchiveFormat: Send + Sync + std::fmt::Debug {
    /// Which format this is.
    fn id(&self) -> FormatId;

    /// How it accepts writes.
    fn write_model(&self) -> WriteModel;

    /// How a member's bytes are obtained.
    fn member_source(&self) -> MemberSource {
        MemberSource::Stream
    }

    /// True only for formats whose containers really use `\` as a path
    /// separator. RAR does; on everything else a backslash is an ordinary
    /// character in a Unix filename and rewriting it would rename the user's
    /// file (see [`super::safety::normalize_member`]).
    fn backslash_separators(&self) -> bool {
        false
    }

    /// What the UI may offer on an archive of this format.
    ///
    /// `seekable` is true only for a format whose reads are materialised: a
    /// streamed member has no random access, which is exactly the case
    /// the forward-only viewer mode exists for. `random_access`
    /// is false everywhere, because even a materialised read pays for the
    /// first one.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            writable: self.write_model().writable(),
            seekable: matches!(self.member_source(), MemberSource::Materialise),
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

    /// Read every member's metadata out of `container`, pushing each into
    /// `sink` **as it is read**.
    ///
    /// Must not buffer the whole listing before pushing: the panel fills from
    /// this. Must stop promptly and return `Ok(())` once `sink.push` answers
    /// `false`. Runs on its own thread; blocking is expected.
    fn index(&self, container: &Path, sink: &mut dyn IndexSink) -> Result<()>;

    /// Write one member's bytes into `out`, returning how many.
    ///
    /// Implemented by every format whose [`ArchiveFormat::member_source`] is
    /// [`MemberSource::Stream`]. A `Materialise` format implements
    /// [`ArchiveFormat::extract_member`] instead and leaves this refusing.
    fn read_member(&self, container: &Path, member: &Member, out: &mut dyn Write) -> Result<u64> {
        let _ = (container, member, out);
        Err(Error::Unsupported("streaming a member of this format"))
    }

    /// Extract one member to a local file.
    ///
    /// The default streams [`ArchiveFormat::read_member`] into it, which is
    /// right for every format that can stream. `.rar` overrides it because
    /// `unrar` extracts to a path or to a `Vec<u8>` and nothing else.
    ///
    /// The caller owns `dest` and is responsible for it being inside a
    /// directory it controls; this is not where the escape rule is
    /// enforced - that is [`super::safety`], at index time and again at
    /// extraction time.
    fn extract_member(&self, container: &Path, member: &Member, dest: &Path) -> Result<u64> {
        let mut file = std::fs::File::create(dest).map_err(|e| Error::io(dest, e))?;
        let written = self.read_member(container, member, &mut file)?;
        file.flush().map_err(|e| Error::io(dest, e))?;
        Ok(written)
    }

    /// Apply `edits` to an existing archive.
    ///
    /// A [`WriteModel::Member`] format edits in place. A
    /// [`WriteModel::FullRewrite`] format writes a new container beside the
    /// original and renames over it **only on success**, so an interrupted
    /// rewrite never destroys the archive; the size and free-space gates
    /// are the caller's, checked before a byte is touched.
    fn apply(
        &self,
        container: &Path,
        edits: &[MemberEdit],
        progress: &mut dyn WriteProgress,
    ) -> Result<()> {
        let _ = (container, edits, progress);
        Err(Error::Unsupported("writing to this archive format"))
    }

    /// Let go of anything this format is holding for `container`.
    ///
    /// Called when the last handle on an archive is dropped. Formats are
    /// `'static` and stateless by design, but "stateless" is not "holds
    /// nothing": [`super::tar`] keeps an open decoder positioned part way
    /// through a compressed container so that reading its members in order
    /// costs one decompression rather than one per member, and an `xz`
    /// decoder's window is megabytes. This is where that is given back rather
    /// than at some later, unrelated read.
    ///
    /// The default does nothing, which is right for every format that really
    /// holds nothing.
    fn release(&self, container: &Path) {
        let _ = container;
    }

    /// Can `Alt+F5` create a **new** archive of this format?
    ///
    /// A different question from [`ArchiveFormat::write_model`], which
    /// answers for members of an archive that already exists. The two agree
    /// today only because every member-writable format happens to be a
    /// container: a single-file compressor that learned to rewrite its one
    /// member would become writable without ever becoming something five
    /// files fit into, and a pack offer filtered on `writable()` would then
    /// fail at the job. The pack dialog and the pack runner ask this instead.
    ///
    /// `true` exactly where [`ArchiveFormat::create`] is implemented; the
    /// default matches `create`'s default refusal.
    fn can_create(&self) -> bool {
        false
    }

    /// Create a new archive at `dest` holding `edits` (`Alt+F5`).
    fn create(
        &self,
        dest: &Path,
        edits: &[MemberEdit],
        level: CompressionLevel,
        progress: &mut dyn WriteProgress,
    ) -> Result<()> {
        let _ = (dest, edits, level, progress);
        Err(Error::Unsupported("packing into this archive format"))
    }
}

/// The compression wrapped around a tar.
///
/// Its own type because it is the only axis on which the five tar rows of the
/// table differ: one [`super::tar::TarFormat`] serves all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// A plain `.tar`.
    None,
    /// `flate2`.
    Gzip,
    /// `bzip2`.
    Bzip2,
    /// `liblzma` - **not** `xz2`, which the design rejects.
    Xz,
    /// `zstd`.
    Zstd,
}

impl Compression {
    /// Wrap `reader` in this compression's decoder.
    ///
    /// The multi-stream decoders throughout: `pigz`, `pbzip2` and `zstd -T0`
    /// all produce concatenated streams, and a single-stream decoder stops at
    /// the first one and reports a short archive rather than an error, which
    /// is the worst of both.
    pub fn decoder<R: Read + Send + 'static>(self, reader: R) -> Result<Box<dyn Read + Send>> {
        Ok(match self {
            Self::None => Box::new(reader),
            Self::Gzip => Box::new(flate2::read::MultiGzDecoder::new(reader)),
            Self::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(reader)),
            Self::Xz => Box::new(liblzma::read::XzDecoder::new_multi_decoder(reader)),
            Self::Zstd => Box::new(zstd::stream::read::Decoder::new(reader).map_err(Error::Bare)?),
        })
    }

    /// Wrap `writer` in this compression's encoder.
    pub fn encoder<W: Write + Send + 'static>(
        self,
        writer: W,
        level: CompressionLevel,
    ) -> Result<Box<dyn Write + Send>> {
        Ok(match self {
            Self::None => Box::new(writer),
            Self::Gzip => Box::new(flate2::write::GzEncoder::new(
                writer,
                flate2::Compression::new(u32::from(level.get())),
            )),
            Self::Bzip2 => Box::new(bzip2::write::BzEncoder::new(
                writer,
                bzip2::Compression::new(u32::from(level.get().max(1))),
            )),
            Self::Xz => Box::new(liblzma::write::XzEncoder::new(
                writer,
                u32::from(level.get()),
            )),
            Self::Zstd => Box::new(
                zstd::stream::write::Encoder::new(writer, level.scaled(1, 19))
                    .map_err(Error::Bare)?
                    .auto_finish(),
            ),
        })
    }

    /// The magic bytes this compression starts with, if any.
    pub const fn magic(self) -> &'static [u8] {
        match self {
            Self::None => &[],
            Self::Gzip => &[0x1f, 0x8b],
            Self::Bzip2 => b"BZh",
            Self::Xz => &[0xfd, b'7', b'z', b'X', b'Z', 0x00],
            Self::Zstd => &[0x28, 0xb5, 0x2f, 0xfd],
        }
    }
}

/// What the first bytes of a file say it is, before the question of what is
/// *inside* a compressed stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Container {
    Format(FormatId),
    Compressed(Compression),
}

/// The magic-byte table for the eight formats.
///
/// `infer` is not among this milestone's dependencies (the design lists it,
/// but for `Open with…` typing in v0.7, not here) and eight formats is a short
/// table, so it is written out. Order matters only in that nothing here is a
/// prefix of anything else.
const MAGIC: &[(&[u8], Container)] = &[
    // Local file header, end-of-central-directory (an empty zip), and the
    // spanned-archive marker. A zip with a self-extracting stub in front of it
    // matches none of these and falls through to the extension.
    (b"PK\x03\x04", Container::Format(FormatId::Zip)),
    (b"PK\x05\x06", Container::Format(FormatId::Zip)),
    (b"PK\x07\x08", Container::Format(FormatId::Zip)),
    (
        &[0x37, 0x7a, 0xbc, 0xaf, 0x27, 0x1c],
        Container::Format(FormatId::SevenZ),
    ),
    // RAR 5, then RAR 1.5-4. The v5 signature extends the v4 one, so it is
    // tried first.
    (b"Rar!\x1a\x07\x01\x00", Container::Format(FormatId::Rar)),
    (b"Rar!\x1a\x07\x00", Container::Format(FormatId::Rar)),
    (&[0x1f, 0x8b], Container::Compressed(Compression::Gzip)),
    (b"BZh", Container::Compressed(Compression::Bzip2)),
    (
        &[0xfd, b'7', b'z', b'X', b'Z', 0x00],
        Container::Compressed(Compression::Xz),
    ),
    (
        &[0x28, 0xb5, 0x2f, 0xfd],
        Container::Compressed(Compression::Zstd),
    ),
];

/// The most an *empty* tar is allowed to be for [`is_empty_tar`] to say so.
///
/// An empty tar is the end-of-archive marker and nothing else: two zero blocks,
/// padded out to whatever block factor wrote it - 10 KiB for GNU `tar`'s
/// default, 20 blocks. A megabyte is far past any of them, and having a bound
/// at all is what stops "the first 512 bytes are zero" from turning a
/// zero-filled disk image into an archive.
const MAX_EMPTY_TAR: u64 = 1024 * 1024;

/// Is this whole file an **empty** tar?
///
/// A tar with no members is its end-of-archive marker: zero bytes, a whole
/// number of 512-byte blocks of them. GNU `tar` writes one for
/// `tar cf empty.tar --files-from=/dev/null` and reads it back without
/// complaint, so refusing to open one is a bug on its own - and it is also
/// exactly what the `Alt+F5` creates before it puts anything in.
///
/// [`looks_like_tar`] cannot answer this and must not be taught to: it is given
/// one block and asked whether a *header* is in it, and an all-zero block is
/// precisely the thing that is not a header. So this is a separate question
/// asked of the whole file, and it is asked last - after every magic number and
/// after the checksum heuristic - so nothing that is really something else can
/// reach it.
fn is_empty_tar(file: &mut std::fs::File, total: u64) -> bool {
    if total == 0 || !total.is_multiple_of(TAR_BLOCK as u64) || total > MAX_EMPTY_TAR {
        return false;
    }
    if file.seek(SeekFrom::Start(0)).is_err() {
        return false;
    }
    all_zero(file, total)
}

/// Does `reader` produce exactly `expected` bytes, every one of them zero?
fn all_zero(reader: &mut dyn Read, expected: u64) -> bool {
    let mut block = [0u8; TAR_BLOCK];
    let mut seen: u64 = 0;
    loop {
        let read = match read_as_much_as(reader, &mut block) {
            Ok(read) => read,
            Err(_) => return false,
        };
        if read == 0 {
            return seen == expected;
        }
        if block.get(..read).unwrap_or(&[]).iter().any(|b| *b != 0) {
            return false;
        }
        seen = seen.saturating_add(read as u64);
        if seen > expected {
            return false;
        }
    }
}

/// Does this block look like the first header of a tar?
///
/// Two independent answers, because both kinds of tar exist:
///
/// * the POSIX/GNU `ustar` marker at offset 257, and
/// * a valid header checksum, which is what a pre-POSIX v7 tar has instead of
///   a magic number. Checking it is what stops "any 512 bytes" from being read
///   as a tar.
pub fn looks_like_tar(block: &[u8]) -> bool {
    let Some(magic) = block.get(257..262) else {
        return false;
    };
    if magic == b"ustar" {
        return true;
    }
    // The checksum field is bytes 148..156, and is computed over the header
    // with those eight bytes taken as spaces.
    let Some(field) = block.get(148..156) else {
        return false;
    };
    let text = field
        .iter()
        .map(|b| *b as char)
        .collect::<String>()
        .trim_matches(|c: char| c == '\0' || c == ' ')
        .to_string();
    if text.is_empty() {
        return false;
    }
    let Ok(claimed) = u32::from_str_radix(&text, 8) else {
        return false;
    };
    let Some(header) = block.get(..TAR_BLOCK) else {
        return false;
    };
    let mut unsigned: u32 = 0;
    let mut signed: i32 = 0;
    for (i, byte) in header.iter().enumerate() {
        let value = if (148..156).contains(&i) { b' ' } else { *byte };
        unsigned = unsigned.wrapping_add(u32::from(value));
        signed = signed.wrapping_add(i32::from(value as i8));
    }
    // Some historical tars computed the checksum over signed chars; both
    // answers are accepted, which is what every tar implementation does.
    claimed == unsigned || i64::from(claimed) == i64::from(signed)
}

/// The tar format that sits inside `compression`.
const fn tar_in(compression: Compression) -> FormatId {
    match compression {
        Compression::None => FormatId::Tar,
        Compression::Gzip => FormatId::TarGz,
        Compression::Bzip2 => FormatId::TarBz2,
        Compression::Xz => FormatId::TarXz,
        Compression::Zstd => FormatId::TarZst,
    }
}

/// What `head` says the container is, before looking inside a compressed
/// stream. `None` when nothing matched.
fn sniff(head: &[u8]) -> Option<Container> {
    for (magic, container) in MAGIC {
        if head.starts_with(magic) {
            return Some(*container);
        }
    }
    if looks_like_tar(head) {
        return Some(Container::Format(FormatId::Tar));
    }
    None
}

/// The single-stream row for a compression, the twin of [`tar_in`].
const fn single_in(compression: Compression) -> FormatId {
    match compression {
        // A `Compression::None` stream is a plain tar and never reaches here;
        // the arm exists because the enum is matched exhaustively everywhere.
        Compression::None => FormatId::Tar,
        Compression::Gzip => FormatId::Gz,
        Compression::Bzip2 => FormatId::Bz2,
        Compression::Xz => FormatId::Xz,
        Compression::Zstd => FormatId::Zst,
    }
}

/// Whether a head of bytes is a container this program can browse.
///
/// The question `Enter` cannot ask in `dispatch`, which may not read. It is
/// deliberately narrower than [`detect`]: a definite container magic only, and
/// not a compressed stream, because deciding whether a gzip holds a tar means
/// decompressing it and that is more than a keystroke should spend guessing.
/// A `.tgz` is recognised by its name on the earlier path, and `Ctrl+PgDn`
/// remains the explicit answer for anything neither route catches.
#[must_use]
pub fn head_is_container(head: &[u8]) -> bool {
    matches!(sniff(head), Some(Container::Format(_)))
}

/// Identify the archive at `path`: **content first, extension second**.
///
///
/// Returns [`Error::Unsupported`] for a file that is not one of the eight, and
/// says which of the two questions failed - a `.zip` that is not a zip and a
/// file with no extension that is not an archive are different problems and
/// the message says which one happened.
pub fn detect(path: &Path) -> Result<FormatId> {
    let mut file = std::fs::File::open(path).map_err(|e| Error::io(path, e))?;
    let mut head = [0u8; SNIFF_LEN];
    let read = read_as_much_as(&mut file, &mut head)?;
    let head = head.get(..read).unwrap_or(&[]);

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    match sniff(head) {
        Some(Container::Format(id)) => Ok(id),
        Some(Container::Compressed(compression)) => {
            // gzip magic says "gzip", not "a tar inside gzip". the design's
            // table has no row for a singly compressed file, so the honest
            // answer for `notes.txt.gz` is "unsupported", not "an empty
            // archive".
            if compressed_holds_a_tar(path, compression)? {
                Ok(tar_in(compression))
            } else {
                // Not a refusal any more. A stream that is not a tar is one
                // compressed file, and one compressed file is a container of
                // exactly one member - which is what makes a `.img.xz` open,
                // view and unpack like everything else. See
                // [`super::single`].
                Ok(single_in(compression))
            }
        }
        None => match FormatId::from_name(&name) {
            // The content said nothing, so the name is all there is. A zip
            // with a self-extracting stub in front of it lands here and works.
            Some(id) => Ok(id),
            None if std::fs::metadata(path).is_ok_and(|m| is_empty_tar(&mut file, m.len())) => {
                Ok(FormatId::Tar)
            }
            None => Err(Error::msg(format!(
                "{}: not a supported archive - its contents match none of {} and \
 its name suggests nothing either",
                path.display(),
                FormatId::ALL
                    .iter()
                    .map(|f| f.label())
                    .collect::<Vec<_>>()
                    .join(", "),
            ))),
        },
    }
}

/// Decompress just enough of `path` to see whether a tar header is in there.
///
/// One block. A corrupt or hostile stream cannot make this read more than
/// [`TAR_BLOCK`] bytes of output, so a decompression bomb costs a kilobyte.
fn compressed_holds_a_tar(path: &Path, compression: Compression) -> Result<bool> {
    let file = std::fs::File::open(path).map_err(|e| Error::io(path, e))?;
    let mut decoder = match compression.decoder(file) {
        Ok(decoder) => decoder,
        // A stream we cannot even start decoding is not a compressed tar; say
        // so rather than propagating a decoder-internal error.
        Err(_) => return Ok(false),
    };
    let mut block = [0u8; TAR_BLOCK];
    let read = match read_as_much_as(&mut decoder, &mut block) {
        Ok(read) => read,
        Err(_) => return Ok(false),
    };
    let head = block.get(..read).unwrap_or(&[]);
    if looks_like_tar(head) {
        return Ok(true);
    }
    // An **empty** tar: the end-of-archive marker and nothing else. See
    // [`is_empty_tar`] - the same question, asked of a decompressed stream
    // whose length is not known until it ends, so the bound is on the output
    // rather than on the file. A bomb therefore costs a megabyte here and not
    // a byte more.
    if read < TAR_BLOCK || head.iter().any(|b| *b != 0) {
        return Ok(false);
    }
    let mut seen = read as u64;
    let mut rest = decoder.take(MAX_EMPTY_TAR);
    loop {
        let read = match read_as_much_as(&mut rest, &mut block) {
            Ok(read) => read,
            Err(_) => return Ok(false),
        };
        if read == 0 {
            return Ok(seen.is_multiple_of(TAR_BLOCK as u64));
        }
        if block.get(..read).unwrap_or(&[]).iter().any(|b| *b != 0) {
            return Ok(false);
        }
        seen = seen.saturating_add(read as u64);
        if seen > MAX_EMPTY_TAR {
            return Ok(false);
        }
    }
}

/// Fill `buf` as far as the reader allows, tolerating short reads.
///
/// `read` is allowed to return fewer bytes than asked for at any time, and a
/// decoder returns very few on its first call. Taking the first answer as the
/// whole answer is how a sniffer starts reporting nonsense on a slow pipe.
fn read_as_much_as(reader: &mut dyn Read, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(buf.get_mut(filled..).unwrap_or(&mut [])) {
            Ok(0) => break,
            Ok(n) => filled = filled.saturating_add(n),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Error::Bare(e)),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_format_has_a_backend_and_a_consistent_write_model() {
        for id in FormatId::ALL {
            let backend = id.backend();
            assert_eq!(backend.id(), *id, "{id} resolves to its own backend");
            assert_eq!(
                backend.capabilities().writable,
                backend.write_model().writable(),
                "{id}: capabilities and the write model must agree"
            );
        }
    }

    #[test]
    fn rar_is_read_only_and_says_so_up_front() {
        // RAR is patent-encumbered for writing, so `F5` into one
        // is refused by `Capabilities` before a byte moves.
        let rar = FormatId::Rar.backend();
        assert_eq!(rar.write_model(), WriteModel::None);
        assert!(!rar.capabilities().writable);
        assert!(matches!(
            rar.apply(Path::new("/x.rar"), &[], &mut NoProgress),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn creating_is_a_narrower_capability_than_writing() {
        // `can_create` implies a writable model, never the other way round:
        // a format an archive can be made of must also accept the write.
        for id in FormatId::ALL {
            let backend = id.backend();
            assert!(
                !backend.can_create() || backend.write_model().writable(),
                "{id}: creatable formats accept writes"
            );
        }
        // `.rar` cannot be written at all, and the single-file compressors
        // are one stream rather than a container: none of them is a pack
        // target, whatever their write model becomes.
        for id in [
            FormatId::Rar,
            FormatId::Gz,
            FormatId::Bz2,
            FormatId::Xz,
            FormatId::Zst,
        ] {
            assert!(!id.backend().can_create(), "{id} is not a pack target");
        }
        // Every container that accepts writes can also be made from nothing.
        for id in [
            FormatId::Zip,
            FormatId::Tar,
            FormatId::TarGz,
            FormatId::TarBz2,
            FormatId::TarXz,
            FormatId::TarZst,
            FormatId::SevenZ,
        ] {
            assert!(id.backend().can_create(), "{id} can be created");
        }
    }

    #[test]
    fn only_compressed_tars_need_the_rewrite_gates() {
        // ".zip and uncompressed .tar support member-level
        // writes and are subject to neither gate."
        assert!(!FormatId::Zip.backend().write_model().needs_rewrite_gates());
        assert!(!FormatId::Tar.backend().write_model().needs_rewrite_gates());
        for id in [
            FormatId::TarGz,
            FormatId::TarBz2,
            FormatId::TarXz,
            FormatId::TarZst,
        ] {
            assert!(
                id.backend().write_model().needs_rewrite_gates(),
                "{id} is a full rewrite"
            );
        }
    }

    #[test]
    fn the_longest_extension_wins() {
        assert_eq!(FormatId::from_name("a.tar.gz"), Some(FormatId::TarGz));
        assert_eq!(FormatId::from_name("a.TAR.GZ"), Some(FormatId::TarGz));
        assert_eq!(FormatId::from_name("a.tgz"), Some(FormatId::TarGz));
        assert_eq!(FormatId::from_name("a.tar.zst"), Some(FormatId::TarZst));
        assert_eq!(FormatId::from_name("a.tar"), Some(FormatId::Tar));
        assert_eq!(FormatId::from_name("a.7z"), Some(FormatId::SevenZ));
        assert_eq!(FormatId::from_name("a.rar"), Some(FormatId::Rar));
        assert_eq!(FormatId::from_name("a.txt"), None);
        // A bare compression suffix is a container now: one compressed file,
        // presented as an archive of one member. The point of the ordering is
        // that `a.tar.gz` above is still a tar and not one of these.
        assert_eq!(FormatId::from_name("a.gz"), Some(FormatId::Gz));
        assert_eq!(FormatId::from_name("a.xz"), Some(FormatId::Xz));
        assert_eq!(FormatId::from_name("disk.img.xz"), Some(FormatId::Xz));
        assert_eq!(FormatId::from_name("a.bz2"), Some(FormatId::Bz2));
        assert_eq!(FormatId::from_name("a.zst"), Some(FormatId::Zst));
    }

    #[test]
    fn magic_beats_the_extension() {
        let mut head = Vec::from(b"PK\x03\x04".as_slice());
        head.resize(SNIFF_LEN, 0);
        assert_eq!(sniff(&head), Some(Container::Format(FormatId::Zip)));
        assert_eq!(
            sniff(&[0x1f, 0x8b, 0x08, 0x00]),
            Some(Container::Compressed(Compression::Gzip))
        );
        assert_eq!(
            sniff(b"BZh9"),
            Some(Container::Compressed(Compression::Bzip2))
        );
        assert_eq!(
            sniff(&[0xfd, b'7', b'z', b'X', b'Z', 0x00, 0x00]),
            Some(Container::Compressed(Compression::Xz))
        );
        assert_eq!(
            sniff(&[0x28, 0xb5, 0x2f, 0xfd, 0x00]),
            Some(Container::Compressed(Compression::Zstd))
        );
        assert_eq!(
            sniff(&[0x37, 0x7a, 0xbc, 0xaf, 0x27, 0x1c]),
            Some(Container::Format(FormatId::SevenZ))
        );
        assert_eq!(
            sniff(b"Rar!\x1a\x07\x01\x00"),
            Some(Container::Format(FormatId::Rar))
        );
        assert_eq!(
            sniff(b"Rar!\x1a\x07\x00"),
            Some(Container::Format(FormatId::Rar))
        );
        assert_eq!(sniff(b"hello, world"), None);
    }

    #[test]
    fn a_tar_header_is_recognised_by_magic_or_by_checksum() {
        let mut block = [0u8; TAR_BLOCK];
        block[257..262].copy_from_slice(b"ustar");
        assert!(looks_like_tar(&block));

        // A v7 tar has no `ustar`, only a checksum.
        let mut v7 = [0u8; TAR_BLOCK];
        v7[..5].copy_from_slice(b"a.txt");
        let mut sum: u32 = 0;
        for (i, byte) in v7.iter().enumerate() {
            let value = if (148..156).contains(&i) { b' ' } else { *byte };
            sum = sum.wrapping_add(u32::from(value));
        }
        let text = format!("{sum:06o}\0 ");
        v7[148..156].copy_from_slice(text.as_bytes());
        assert!(looks_like_tar(&v7), "a v7 tar is a tar");

        v7[0] = b'z'; // breaks the checksum
        assert!(!looks_like_tar(&v7));
        assert!(
            !looks_like_tar(&[0u8; TAR_BLOCK]),
            "512 zeroes is not a tar"
        );
        assert!(!looks_like_tar(b"short"));
    }

    #[test]
    fn compression_levels_clamp_and_scale() {
        assert_eq!(CompressionLevel::new(200).get(), 9);
        assert_eq!(CompressionLevel::DEFAULT.get(), 6);
        assert_eq!(CompressionLevel::STORE.scaled(1, 19), 1);
        assert_eq!(CompressionLevel::MAX.scaled(1, 19), 19);
        assert!(CompressionLevel::DEFAULT.scaled(1, 19) > 1);
    }
}
