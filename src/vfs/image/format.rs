//! What is in an image, decided by reading it.
//!
//! > An extension is a hint and never the answer, since `.img` says nothing at
//! > all about what is inside.
//!
//! [`detect`] never sees a file name. [`looks_like_image_name`] exists for one
//! question that has to be answered without reading anything - whether `Enter`
//! should try - and for no other (the design I14).

use std::io::Write;
use std::path::Path;

use crate::error::{Error, Result};
use crate::vfs::archive::index::{IndexSink, Member};
use crate::vfs::{Capabilities, ReadSeek};

use super::block::{self, Region};
use super::part::{self, Table};

/// The deepest a directory walk goes before it stops.
///
/// A FAT directory entry names a cluster and nothing in the format stops that
/// cluster from being an ancestor's; an ISO 9660 Rock Ridge `CL` entry names
/// a sector and nothing stops that sector from being one already visited.
/// Neither library checks. `index::MAX_MEMBERS` and `index::MAX_NAME_BYTES`
/// bound the damage either way, and this bounds the time
/// (the design I8).
pub const MAX_DIR_DEPTH: usize = 64;

/// A filesystem, named.
///
/// Every variant is something a superblock can be recognised as, whether or
/// not this program can read it: the design requires an unsupported
/// filesystem to be "reported by name" rather than as a damaged image, and a
/// name it cannot produce is a name it cannot report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsId {
    /// ISO 9660, with Joliet and Rock Ridge where the image carries them.
    Iso9660,
    /// FAT12. Only distinguishable from FAT16 after the volume is opened.
    Fat12,
    /// FAT16.
    Fat16,
    /// FAT32.
    Fat32,
    /// exFAT. Recognised, not supported: the design records that there is
    /// no exFAT crate worth depending on.
    ExFat,
    /// NTFS. Recognised, not supported: the design names the `ntfs` crate
    /// as the obvious candidate and records that adding it is a decision
    /// rather than an oversight.
    Ntfs,
    /// ext2, or ext3/ext4 that could not be told apart.
    Ext2,
    /// ext3.
    Ext3,
    /// ext4.
    Ext4,
    /// HFS+.
    HfsPlus,
    /// An APFS container.
    Apfs,
    /// SquashFS.
    Squashfs,
    /// Linux swap, which has no files in it at all.
    LinuxSwap,
    /// A signature nothing recognised.
    Unknown,
}

impl FsId {
    /// The name a user is shown.
    pub fn label(self) -> &'static str {
        match self {
            Self::Iso9660 => "ISO 9660",
            Self::Fat12 => "FAT12",
            Self::Fat16 => "FAT16",
            Self::Fat32 => "FAT32",
            Self::ExFat => "exFAT",
            Self::Ntfs => "NTFS",
            Self::Ext2 => "ext2",
            Self::Ext3 => "ext3",
            Self::Ext4 => "ext4",
            Self::HfsPlus => "HFS+",
            Self::Apfs => "APFS",
            Self::Squashfs => "SquashFS",
            Self::LinuxSwap => "Linux swap",
            Self::Unknown => "an unrecognised filesystem",
        }
    }

    /// Whether this program can read it. True for ISO 9660 and the three
    /// FATs, false for everything else.
    pub fn supported(self) -> bool {
        matches!(
            self,
            Self::Iso9660 | Self::Fat12 | Self::Fat16 | Self::Fat32
        )
    }

    /// The reader, or `None` when there is not one.
    pub fn backend(self) -> Option<&'static dyn VolumeFormat> {
        match self {
            Self::Iso9660 => Some(&super::iso::Iso9660),
            Self::Fat12 | Self::Fat16 | Self::Fat32 => Some(&super::fat::Fat),
            Self::ExFat
            | Self::Ntfs
            | Self::Ext2
            | Self::Ext3
            | Self::Ext4
            | Self::HfsPlus
            | Self::Apfs
            | Self::Squashfs
            | Self::LinuxSwap
            | Self::Unknown => None,
        }
    }

    /// How this filesystem is reported when it is met and cannot be read.
    ///
    /// The three states of the design are kept apart here:
    /// a recognised filesystem is named, an unrecognised one says so, and
    /// neither is the word this crate uses for an image whose structure did
    /// not parse (I13).
    pub fn refusal(self, what: &str) -> Error {
        if self == Self::Unknown {
            return Error::msg(format!("{what}: the filesystem was not recognised"));
        }
        Error::msg(format!("{what}: {}, not supported", self.label()))
    }

    /// What `am-partitions`'s sniffer saw.
    ///
    /// A wildcard arm, deliberately, and the only one in this module: the
    /// house rule is exhaustive matching on *this crate's* enums, and
    /// `partitions::FsKind` belongs to a crate that may add a variant in a
    /// patch release. An unknown signature is [`FsId::Unknown`], which is
    /// already the honest answer.
    pub fn from_sniff(kind: partitions::FsKind) -> Self {
        use partitions::sniff::ExtVersion;
        match kind {
            partitions::FsKind::Iso9660 => Self::Iso9660,
            // The sniffer reads the FAT12 and the FAT16 tag into one variant,
            // and cannot do better: the two differ by cluster count, which is
            // arithmetic over the BPB rather than a signature. `fatfs` answers
            // it exactly once the volume is opened, and that refined answer is
            // what a message names.
            partitions::FsKind::Fat16 => Self::Fat16,
            partitions::FsKind::Fat32 => Self::Fat32,
            partitions::FsKind::ExFat => Self::ExFat,
            partitions::FsKind::Ntfs => Self::Ntfs,
            partitions::FsKind::Ext { version } => match version {
                ExtVersion::Ext2OrAny => Self::Ext2,
                ExtVersion::Ext3 => Self::Ext3,
                ExtVersion::Ext4 => Self::Ext4,
            },
            partitions::FsKind::HfsPlus => Self::HfsPlus,
            partitions::FsKind::Apfs => Self::Apfs,
            partitions::FsKind::Squashfs => Self::Squashfs,
            partitions::FsKind::LinuxSwap => Self::LinuxSwap,
            _ => Self::Unknown,
        }
    }
}

/// What an image turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    /// The image is itself a filesystem and has no partition segment.
    /// "An image with no partition table has no such segment
    /// and the first one is the filesystem's root."
    Volume(FsId),
    /// The image is partitioned, and each partition is a segment.
    Partitioned(Table),
}

/// Decide what `container` is, by reading it.
///
/// The order is total and is argued in
///
/// 1. the signature at offset 0. An image that is itself a filesystem is that
///    filesystem, whatever a hybrid partition table alongside it claims -
///    every installer ISO since 2012 is isohybrid and the own
///    `ubuntu.iso` example is unreachable any other way;
/// 2. GPT, then MBR;
/// 3. neither, which is not a disk image and says so.
///
/// Reads at most [`block::SNIFF_LEN`] bytes plus the table. Never opens a
/// filesystem: what is on a partition is a claim from its superblock until
/// the volume is opened, and being wrong about it costs a message rather than
/// a listing.
pub fn detect(container: &Path) -> Result<Shape> {
    let whole = Region::whole(container)?;
    let at_zero = sniff(&whole)?;
    if at_zero != FsId::Unknown {
        return Ok(Shape::Volume(at_zero));
    }
    match part::read_table(container, whole.len())? {
        Some(table) => Ok(Shape::Partitioned(table)),
        None => Err(Error::InvalidPath(format!(
            "{}: not a disk image",
            container.display()
        ))),
    }
}

/// The filesystem at the start of `region`, from its superblock alone.
pub fn sniff(region: &Region) -> Result<FsId> {
    let window = block::sniff_window(region)?;
    Ok(FsId::from_sniff(partitions::sniff::classify(&window)))
}

/// Whether a name is worth trying to enter as an image.
///
/// `.iso` and `.img`, case-insensitively, and nothing else. This is a hint and
/// is never asked what is inside: [`detect`] never sees a name
/// (the design I14).
pub fn looks_like_image_name(name: &str) -> bool {
    let Some(cut) = name.rfind('.') else {
        return false;
    };
    let Some(ext) = name.get(cut.saturating_add(1)..) else {
        return false;
    };
    ext.eq_ignore_ascii_case("iso") || ext.eq_ignore_ascii_case("img")
}

/// One readable filesystem.
///
/// The sibling of `archive::format::ArchiveFormat`, and deliberately the same
/// shape: implementations are zero-sized and `'static`, everything stateful
/// belongs to [`super::ImageFs`] and the session, and a format cannot see the
/// index, cannot decide what is safe and cannot allocate the storage.
///
/// There is no `apply`, no `create` and no `write_model`. This is not an
/// oversight and is not an omission to be filled in later: the design is
/// "read-only, and not as a first step towards writing", and a trait with no
/// write method is the version of that claim the compiler checks.
pub trait VolumeFormat: Send + Sync + std::fmt::Debug {
    /// Which filesystem this reads.
    fn id(&self) -> FsId;

    /// What the UI may offer on a volume of this filesystem.
    fn capabilities(&self) -> Capabilities;

    /// Read every member's metadata out of `region`, pushing each into `sink`
    /// as it is read.
    ///
    /// Must not buffer the whole listing before pushing: the panel fills from
    /// this. Must stop promptly and return `Ok(())` once `sink.push` answers
    /// `false`. Runs on its own thread; blocking is expected. Must bound its
    /// own recursion at [`MAX_DIR_DEPTH`], because a directory in a crafted
    /// image can be its own ancestor and neither library checks
    /// (the design I8).
    fn index(&self, region: &Region, sink: &mut dyn IndexSink) -> Result<()>;

    /// Write one member's bytes into `out`, returning how many.
    fn read_member(&self, region: &Region, member: &Member, out: &mut dyn Write) -> Result<u64>;

    /// A seekable handle on one member, where the filesystem has one.
    ///
    /// `Ok(None)` means "this member is not randomly accessible", which the
    /// viewer already handles (the forward-only mode). ISO 9660
    /// answers `Some` for a single-extent member, because such a member is a
    /// contiguous byte range of a local file and seeking it is a `pread`;
    /// FAT answers `None`, because a `fatfs::File` borrows a `FileSystem`
    /// that cannot outlive the call.
    fn open_member(
        &self,
        region: &Region,
        member: &Member,
    ) -> Result<Option<Box<dyn ReadSeek + Send>>> {
        let _ = (region, member);
        Ok(None)
    }

    /// The volume label, for a status line. `None` when there is not one.
    fn volume_label(&self, region: &Region) -> Option<String> {
        let _ = region;
        None
    }
}
