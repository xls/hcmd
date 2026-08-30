//! The partition table (GPT and MBR, through
//! `am-partitions`).
//!
//! A partition table is not a filesystem and is not browsed: it produces the
//! listing at the image's own root, one row per partition, and each row's
//! `Entry::location` is the root of the filesystem on it. Entering a row is
//! therefore an ordinary navigation and the segment it pushes is the one
//! the design asks for.

use std::path::Path;

use crate::error::{Error, Result};
use crate::panel::format::human_size;
use crate::vfs::{BackendKind, Entry, VfsPath};

use super::block::Region;
use super::format::{self, FsId};

/// The most partitions any listing will show.
///
/// A GPT header declares its own entry count and a hostile one can declare
/// millions; `am-partitions` bounds what it parses, and this bounds what is
/// kept. 128 is the GPT default and is four times what any real MBR chain
/// reaches.
pub const MAX_PARTITIONS: usize = 128;

/// Which table an image turned out to have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
    /// GPT, read from LBA 1 with its CRCs checked.
    Gpt,
    /// MBR, read from LBA 0. Primary entries only: `am-partitions` does not
    /// walk the extended chain and neither does this.
    ///
    Mbr,
}

impl TableKind {
    /// `GPT`, `MBR`. For a message, never for a decision.
    pub fn label(self) -> &'static str {
        match self {
            Self::Gpt => "GPT",
            Self::Mbr => "MBR",
        }
    }

    /// What `am-partitions` reported.
    ///
    /// Matched arm for arm and with no wildcard: `partitions::TableKind` has
    /// exactly these two variants today and is not `#[non_exhaustive]`, so a
    /// wildcard would be unreachable code that the gate refuses, and a third
    /// variant in a later release should stop this file compiling rather than
    /// be read silently as MBR.
    fn from_probe(kind: partitions::TableKind) -> Self {
        match kind {
            partitions::TableKind::Gpt => Self::Gpt,
            partitions::TableKind::Mbr => Self::Mbr,
        }
    }
}

/// One partition, as this backend addresses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionEntry {
    /// Its number, from 1, by position in the table as read. Not necessarily
    /// the kernel's device number.
    pub number: usize,
    /// Where its bytes are, already checked against the container's length.
    pub region: Region,
    /// What its superblock says is on it. [`FsId::Unknown`] when nothing
    /// recognised it, which is not the same as damaged.
    pub fs: FsId,
    /// The GPT partition name, or `None` for MBR, which has no such field.
    pub label: Option<String>,
    /// The on-disk type, for a message: `EFI System`, `Linux filesystem`,
    /// `0x83`.
    pub type_label: String,
    /// Marked bootable by the table: MBR's active flag, or GPT's ESP type
    /// GUID or legacy-bootable attribute.
    pub bootable: bool,
}

impl PartitionEntry {
    /// The panel row: a directory named for the number, whose real home is
    /// the root of the filesystem on it.
    ///
    /// The name is the number and nothing else. It is what `Ctrl+C` copies,
    /// what quick search matches, what a copy names its destination after and
    /// what `VfsPath::join` builds a path from, and a decorated name would
    /// leak the decoration into all four.
    ///
    /// `image` is the table's own address, so the location this builds is the
    /// three-segment form: the container, this partition, and the root of
    /// what is on it.
    pub fn row(&self, image: &VfsPath) -> Entry {
        let name = self.number.to_string();
        let location = image
            .with_tail(format!("/{name}"))
            .with_segment(BackendKind::Image, "/");
        Entry {
            size: self.region.len(),
            location: Some(location),
            ..Entry::dir(name)
        }
    }

    /// `partition 2, FAT32, 512M, bootable`. For a message and a status line.
    pub fn describe(&self) -> String {
        let mut text = format!(
            "partition {}, {}, {}",
            self.number,
            self.fs.label(),
            human_size(self.region.len()),
        );
        if let Some(label) = self.label.as_deref().filter(|l| !l.is_empty()) {
            text.push_str(&format!(", {label}"));
        }
        if self.bootable {
            text.push_str(", bootable");
        }
        text
    }
}

/// An image's partition table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    /// Which kind it is.
    pub kind: TableKind,
    /// Its partitions, in table order, numbered from 1, at most
    /// [`MAX_PARTITIONS`] of them.
    pub partitions: Vec<PartitionEntry>,
    /// The partitions the table described and this backend will not address,
    /// each already phrased for a human and naming its number.
    ///
    /// **A deviation from the design, recorded rather than hidden.** I3
    /// requires that a partition whose window is not inside the container is
    /// "refused by number with the reason" while "its siblings still list",
    /// and the design gives the sentence for that case and for an extended
    /// partition. A [`PartitionEntry`] whose `region` could not be built
    /// cannot exist, so the reason is kept here: the siblings list, the
    /// numbering does not shift under them, and the table view sends these
    /// after its rows exactly as the archive backend reports the entries it
    /// refused.
    pub refused: Vec<String>,
}

impl Table {
    /// The partition with that number.
    pub fn get(&self, number: usize) -> Option<&PartitionEntry> {
        self.partitions.iter().find(|p| p.number == number)
    }

    /// Every row, for the listing at the image's root.
    pub fn rows(&self, image: &VfsPath) -> Vec<Entry> {
        self.partitions.iter().map(|p| p.row(image)).collect()
    }

    /// Why partition `number` is not in the listing, if that is why it is not.
    pub fn refusal(&self, number: usize) -> Option<&str> {
        let wanted = format!("partition {number}:");
        self.refused
            .iter()
            .find(|why| why.starts_with(&wanted))
            .map(String::as_str)
    }

    /// How many partitions this listing shows, for a status line.
    pub fn len(&self) -> usize {
        self.partitions.len()
    }

    /// Whether the table described nothing this backend can address.
    pub fn is_empty(&self) -> bool {
        self.partitions.is_empty()
    }
}

/// Read the table, if there is one.
///
/// `Ok(None)` means there is no GPT and no MBR signature, which is not an
/// error: the `ubuntu.iso` has no table and is not damaged.
/// `Err` means a table was found and could not be trusted, which is a damaged
/// image and is a different thing to tell the user.
///
///
/// Each partition's superblock is sniffed here, which is the design's
/// "then each partition's superblock", and costs one bounded read per
/// partition.
pub fn read_table(container: &Path, size: u64) -> Result<Option<Table>> {
    // A table on a file with no room for one is not a table. `probe` reads
    // LBA 0 unconditionally and a shorter file would fail as an I/O error,
    // which is the wrong answer: there is simply nothing there.
    if size < 512 {
        return Ok(None);
    }
    let device = partitions::FileBlock::open(container).map_err(|e| {
        Error::msg(format!(
            "{}: could not read the partition table: {e}",
            container.display()
        ))
    })?;
    let (kind, found) = match partitions::probe(&device) {
        Ok(probed) => probed,
        Err(partitions::Error::NoPartitionTable) => return Ok(None),
        Err(err) => {
            return Err(Error::msg(format!(
                "{}: the partition table is damaged: {err}",
                container.display()
            )));
        }
    };

    let whole = Region::whole(container)?;
    let mut partitions = Vec::new();
    let mut refused = Vec::new();
    // Numbered by **position in the table as read**, never by how many
    // survived: skipping an entry and then counting the survivors would move
    // partition 3 to 2 the moment partition 2 was refused, and the number is
    // the address the listing, `Ctrl+G` and every job inside it agree on.
    //
    for (position, found) in found.iter().take(MAX_PARTITIONS).enumerate() {
        let number = position.saturating_add(1);
        // An entry with no bytes is a slot, not a partition. `am-partitions`
        // already drops the empty MBR slots; a GPT entry array can still
        // carry an unused entry that got this far.
        if found.length == 0 || is_unused(found) {
            continue;
        }
        // Refused rather than clamped: a partition that runs off the end of
        // the container is a truncated download or a hostile table, and
        // showing a row for bytes that are not there would fail later, with
        // less to say (the design I3).
        let Ok(region) = whole.slice(found.start, found.length) else {
            refused.push(format!(
                "partition {number}: the partition table describes bytes past \
                 the end of the image"
            ));
            continue;
        };
        // `am-partitions` reports an extended entry as an ordinary partition
        // and does not walk what is behind it, so it is refused by name rather
        // than presented as a partition whose filesystem happens to be
        // unrecognisable.
        if is_extended(found) {
            refused.push(format!(
                "partition {number}: an extended partition, not supported"
            ));
            continue;
        }
        // A claim from the superblock, not a verdict: a partition that says
        // FAT32 and whose FAT is corrupt fails when it is opened, and fails as
        // damaged rather than as unsupported.
        let fs = format::sniff(&region)?;
        partitions.push(PartitionEntry {
            number,
            region,
            fs,
            label: found.label.clone(),
            type_label: type_label(found),
            bootable: found.is_bootable(),
        });
    }

    if partitions.is_empty() && refused.is_empty() {
        // A table whose every entry was empty describes no partition, and a
        // panel with only a `..` row in it would say nothing about why. A
        // table whose entries were refused has its own reasons to show and is
        // not this case.
        return Err(Error::msg(format!(
            "{}: the {} partition table lists no usable partition",
            container.display(),
            TableKind::from_probe(kind).label(),
        )));
    }

    Ok(Some(Table {
        kind: TableKind::from_probe(kind),
        partitions,
        refused,
    }))
}

/// True for the two MBR types that begin an extended chain.
fn is_extended(found: &partitions::Partition) -> bool {
    match found.kind {
        partitions::PartitionKind::Mbr { type_byte, .. } => matches!(
            type_byte,
            partitions::mbr::types::EXTENDED_CHS | partitions::mbr::types::EXTENDED_LBA
        ),
        partitions::PartitionKind::Gpt { .. } | partitions::PartitionKind::Whole => false,
    }
}

/// Whether the table marked this entry as holding nothing.
fn is_unused(found: &partitions::Partition) -> bool {
    match found.kind {
        partitions::PartitionKind::Gpt { type_guid, .. } => {
            type_guid == partitions::gpt::type_guids::UNUSED
        }
        partitions::PartitionKind::Mbr { type_byte, .. } => {
            type_byte == partitions::mbr::types::EMPTY
        }
        partitions::PartitionKind::Whole => false,
    }
}

/// The on-disk type, named where it has a well-known name and printed where
/// it does not.
///
/// A number is a worse answer than a name and a better answer than nothing:
/// an unrecognised GPT type GUID prints as a GUID, which is a thing a user can
/// look up.
fn type_label(found: &partitions::Partition) -> String {
    use partitions::gpt::type_guids as guids;
    use partitions::mbr::types as mbr;

    match found.kind {
        partitions::PartitionKind::Gpt { type_guid, .. } => match type_guid {
            guids::EFI_SYSTEM => "EFI System".to_string(),
            guids::MICROSOFT_BASIC_DATA => "Microsoft basic data".to_string(),
            guids::LINUX_FILESYSTEM => "Linux filesystem".to_string(),
            guids::LINUX_SWAP => "Linux swap".to_string(),
            guids::APPLE_HFS_PLUS => "Apple HFS+".to_string(),
            guids::APPLE_APFS => "Apple APFS".to_string(),
            other => format_guid(other),
        },
        partitions::PartitionKind::Mbr { type_byte, .. } => match type_byte {
            mbr::FAT12 => "FAT12".to_string(),
            mbr::FAT16_SMALL | mbr::FAT16 | mbr::FAT16_LBA => "FAT16".to_string(),
            mbr::NTFS_OR_EXFAT => "NTFS or exFAT".to_string(),
            mbr::FAT32_CHS | mbr::FAT32_LBA => "FAT32".to_string(),
            mbr::EXTENDED_CHS | mbr::EXTENDED_LBA => "extended".to_string(),
            mbr::LINUX_SWAP => "Linux swap".to_string(),
            mbr::LINUX => "Linux".to_string(),
            mbr::LINUX_LVM => "Linux LVM".to_string(),
            mbr::HFS_PLUS => "HFS+".to_string(),
            other => format!("0x{other:02X}"),
        },
        partitions::PartitionKind::Whole => "whole disk".to_string(),
    }
}

/// A GPT type GUID in its usual mixed-endian text form.
///
/// Destructured rather than indexed: the array is sixteen bytes by type, so
/// the pattern is total and there is no bound left to check.
fn format_guid(guid: [u8; 16]) -> String {
    let [
        a0,
        a1,
        a2,
        a3,
        b0,
        b1,
        c0,
        c1,
        d0,
        d1,
        e0,
        e1,
        e2,
        e3,
        e4,
        e5,
    ] = guid;
    let a = u32::from_le_bytes([a0, a1, a2, a3]);
    let b = u16::from_le_bytes([b0, b1]);
    let c = u16::from_le_bytes([c0, c1]);
    format!(
        "{a:08X}-{b:04X}-{c:04X}-{d0:02X}{d1:02X}-{e0:02X}{e1:02X}{e2:02X}{e3:02X}{e4:02X}{e5:02X}"
    )
}

/// The partition a segment tail names: `/2` is 2.
///
/// Refuses anything that is not a single component of decimal digits, so a
/// segment tail cannot smuggle a path in where a number belongs.
pub fn partition_number(tail: &Path) -> Result<usize> {
    let refuse = || Error::InvalidPath(format!("{}: not a partition number", tail.display()));
    let mut components = tail
        .components()
        .filter(|c| !matches!(c, std::path::Component::RootDir));
    let Some(std::path::Component::Normal(only)) = components.next() else {
        return Err(refuse());
    };
    if components.next().is_some() {
        return Err(refuse());
    }
    let text = only.to_str().ok_or_else(refuse)?;
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(refuse());
    }
    let number: usize = text.parse().map_err(|_| refuse())?;
    if number == 0 || number > MAX_PARTITIONS {
        return Err(refuse());
    }
    Ok(number)
}
