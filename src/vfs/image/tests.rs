//! The proof of the design: a disk image is a directory, a partition is a
//! segment, and nothing that reads one can write to it.
//!
//! Every fixture here is built byte by byte in the test. Nothing is checked
//! in and nothing is downloaded: an MBR is 512 bytes, a GPT is a header and an
//! entry array with two CRC32s over them, and a FAT volume is written by the
//! library that reads it. A binary blob in the tree would be a fixture nobody
//! could read, argue with or change.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::vfs::archive::session::RewriteLimits;
use crate::vfs::{BackendKind, EntryKind, Vfs, VfsPath};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A throwaway directory, removed on drop. Built by hand because `tempfile`
/// is not on the dependency table.
struct TempTree {
    root: PathBuf,
}

impl TempTree {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "hcmd-img-{tag}-{}-{nanos:x}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create the temp tree");
        Self { root }
    }

    /// Write `bytes` to `name` inside the tree and hand back its path.
    fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.root.join(name);
        std::fs::write(&path, bytes).expect("write the fixture");
        path
    }

    /// A session whose temp root is inside this tree, so nothing leaks.
    fn session(&self) -> Arc<ArchiveSession> {
        ArchiveSession::in_dir(&self.root, RewriteLimits::default()).expect("session")
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The sector every partition table in this file counts in.
const SECTOR: usize = 512;

/// CRC-32 as GPT defines it, written out rather than depended on.
///
/// `crc32fast` is `am-partitions`'s own dependency and not this crate's, and
/// a GPT fixture needs two of these. Bit-by-bit, reflected, polynomial
/// `0xEDB88320`: eleven lines that a reader can check against the standard
/// instead of a table they would have to trust.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = if crc & 1 != 0 { 0xEDB8_8320 } else { 0 };
            crc = (crc >> 1) ^ mask;
        }
    }
    !crc
}

/// One entry of a hand-built MBR, and the bytes it points at.
struct MbrPart {
    type_byte: u8,
    active: bool,
    start_lba: u32,
    sectors: u32,
    payload: Vec<u8>,
}

impl MbrPart {
    fn new(type_byte: u8, start_lba: u32, sectors: u32) -> Self {
        Self {
            type_byte,
            active: false,
            start_lba,
            sectors,
            payload: Vec::new(),
        }
    }

    fn active(mut self) -> Self {
        self.active = true;
        self
    }

    fn holding(mut self, payload: Vec<u8>) -> Self {
        self.payload = payload;
        self
    }
}

/// Put `bytes` into `image` at `at`, growing it if it has to.
fn splice(image: &mut Vec<u8>, at: usize, bytes: &[u8]) {
    let end = at.saturating_add(bytes.len());
    if image.len() < end {
        image.resize(end, 0);
    }
    if let Some(slot) = image.get_mut(at..end) {
        slot.copy_from_slice(bytes);
    }
}

/// A disk image with a 512-byte MBR and each partition's payload at its LBA.
///
/// The four primary entries and nothing else: `am-partitions` does not walk
/// an extended chain and neither does this backend.
///
fn mbr_image(parts: &[MbrPart]) -> Vec<u8> {
    let mut image = vec![0u8; SECTOR];
    for (slot, part) in parts.iter().take(4).enumerate() {
        let off = 446usize.saturating_add(slot.saturating_mul(16));
        splice(&mut image, off, &[if part.active { 0x80 } else { 0x00 }]);
        splice(&mut image, off.saturating_add(4), &[part.type_byte]);
        splice(
            &mut image,
            off.saturating_add(8),
            &part.start_lba.to_le_bytes(),
        );
        splice(
            &mut image,
            off.saturating_add(12),
            &part.sectors.to_le_bytes(),
        );
    }
    splice(&mut image, 510, &[0x55, 0xAA]);
    for part in parts.iter().take(4) {
        let at = (part.start_lba as usize).saturating_mul(SECTOR);
        // The declared length, whether or not the payload fills it: a
        // partition whose bytes are not there is exactly the case I3 exists
        // for, and a fixture that always grew the file could not produce one.
        if !part.payload.is_empty() {
            splice(&mut image, at, &part.payload);
        }
    }
    image
}

/// One entry of a hand-built GPT.
struct GptPart {
    type_guid: [u8; 16],
    start_lba: u64,
    end_lba: u64,
    attributes: u64,
    name: &'static str,
    payload: Vec<u8>,
}

impl GptPart {
    fn new(type_guid: [u8; 16], start_lba: u64, end_lba: u64) -> Self {
        Self {
            type_guid,
            start_lba,
            end_lba,
            attributes: 0,
            name: "",
            payload: Vec::new(),
        }
    }

    fn named(mut self, name: &'static str) -> Self {
        self.name = name;
        self
    }

    fn holding(mut self, payload: Vec<u8>) -> Self {
        self.payload = payload;
        self
    }
}

/// How many entries a hand-built GPT declares. The default, and what a real
/// header carries.
const GPT_ENTRIES: usize = 128;
/// How many bytes each of them is.
const GPT_ENTRY_SIZE: usize = 128;

/// A disk image with a protective MBR, a GPT header whose CRC32s are right,
/// and an entry array.
///
/// `sectors` is the whole disk, so the backup header has somewhere to be even
/// though nothing here writes one: `am-partitions` treats a missing backup as
/// advisory and the primary is what `probe` reads.
fn gpt_image(sectors: u64, parts: &[GptPart]) -> Vec<u8> {
    let total = usize::try_from(sectors.saturating_mul(SECTOR as u64)).unwrap_or(0);
    let mut image = vec![0u8; total];

    // The protective MBR: one 0xEE entry spanning the disk.
    let off = 446usize;
    splice(&mut image, off.saturating_add(4), &[0xEE]);
    splice(&mut image, off.saturating_add(8), &1u32.to_le_bytes());
    splice(
        &mut image,
        off.saturating_add(12),
        &u32::try_from(sectors.saturating_sub(1))
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    splice(&mut image, 510, &[0x55, 0xAA]);

    // The entry array at LBA 2.
    let mut array = vec![0u8; GPT_ENTRIES.saturating_mul(GPT_ENTRY_SIZE)];
    for (slot, part) in parts.iter().take(GPT_ENTRIES).enumerate() {
        let at = slot.saturating_mul(GPT_ENTRY_SIZE);
        splice(&mut array, at, &part.type_guid);
        // A unique GUID that is merely not zero: nothing here reads it.
        splice(
            &mut array,
            at.saturating_add(16),
            &[u8::try_from(slot.saturating_add(1)).unwrap_or(1); 16],
        );
        splice(
            &mut array,
            at.saturating_add(32),
            &part.start_lba.to_le_bytes(),
        );
        splice(
            &mut array,
            at.saturating_add(40),
            &part.end_lba.to_le_bytes(),
        );
        splice(
            &mut array,
            at.saturating_add(48),
            &part.attributes.to_le_bytes(),
        );
        let name: Vec<u8> = part
            .name
            .encode_utf16()
            .take(35)
            .flat_map(|u| u.to_le_bytes())
            .collect();
        splice(&mut array, at.saturating_add(56), &name);
    }
    splice(&mut image, 2usize.saturating_mul(SECTOR), &array);

    // The header at LBA 1.
    let mut header = vec![0u8; SECTOR];
    splice(&mut header, 0, b"EFI PART");
    splice(&mut header, 8, &0x0001_0000u32.to_le_bytes());
    splice(&mut header, 12, &92u32.to_le_bytes());
    splice(&mut header, 24, &1u64.to_le_bytes());
    splice(&mut header, 32, &sectors.saturating_sub(1).to_le_bytes());
    splice(&mut header, 40, &34u64.to_le_bytes());
    splice(&mut header, 48, &sectors.saturating_sub(34).to_le_bytes());
    splice(&mut header, 56, &[0x11u8; 16]);
    splice(&mut header, 72, &2u64.to_le_bytes());
    splice(
        &mut header,
        80,
        &u32::try_from(GPT_ENTRIES).unwrap_or(128).to_le_bytes(),
    );
    splice(
        &mut header,
        84,
        &u32::try_from(GPT_ENTRY_SIZE).unwrap_or(128).to_le_bytes(),
    );
    splice(&mut header, 88, &crc32(&array).to_le_bytes());
    let checksum = crc32(header.get(..92).unwrap_or(&[]));
    splice(&mut header, 16, &checksum.to_le_bytes());
    splice(&mut image, SECTOR, &header);

    for part in parts {
        let at = usize::try_from(part.start_lba.saturating_mul(SECTOR as u64)).unwrap_or(0);
        if !part.payload.is_empty() {
            splice(&mut image, at, &part.payload);
        }
    }
    image
}

/// A formatted FAT volume, written by `fatfs` itself.
///
/// The one place this module uses `fatfs`'s write path, and it is
/// `#[cfg(test)]`: a fixture written by the library that reads it is honest in
/// a way a hand-rolled boot sector is not.
pub fn fat_image(bytes: usize, files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut disk = std::io::Cursor::new(vec![0u8; bytes]);
    fatfs::format_volume(&mut disk, fatfs::FormatVolumeOptions::new()).expect("format");
    disk.seek(SeekFrom::Start(0)).expect("rewind");
    {
        let fs = fatfs::FileSystem::new(&mut disk, fatfs::FsOptions::new()).expect("open");
        let root = fs.root_dir();
        for (name, bytes) in files {
            let mut file = root.create_file(name).expect("create");
            file.write_all(bytes).expect("write");
            file.flush().expect("flush");
        }
    }
    disk.into_inner()
}

/// A boot sector carrying one signature and nothing else, for the cases where
/// what matters is what the sniffer says rather than what a real driver would.
fn superblock(oem: &[u8; 8]) -> Vec<u8> {
    let mut bytes = vec![0u8; 4096];
    splice(&mut bytes, 3, oem);
    splice(&mut bytes, 510, &[0x55, 0xAA]);
    bytes
}

/// The address of an image's own root: the container, then one `Image`
/// segment at `/`.
fn image_root(container: &std::path::Path) -> VfsPath {
    VfsPath::local(container).with_segment(BackendKind::Image, "/")
}

// --------------------------------------------------------------------------
// Detection by content
// --------------------------------------------------------------------------

/// An image that is itself a filesystem has no partition segment.
#[test]
fn an_unpartitioned_fat_image_is_a_volume() {
    let tree = TempTree::new("fat-volume");
    let path = tree.file("card.img", &fat_image(2 * 1024 * 1024, &[("a.txt", b"hi")]));
    match format::detect(&path).expect("detect") {
        Shape::Volume(fs) => assert!(
            matches!(fs, FsId::Fat12 | FsId::Fat16 | FsId::Fat32),
            "a formatted FAT volume detected as {fs:?}"
        ),
        other => panic!("expected a volume, got {other:?}"),
    }
}

/// A disk with a table is partitioned, and every entry that could be
/// addressed is numbered by its position in the table.
#[test]
fn a_partitioned_image_lists_its_partitions() {
    let tree = TempTree::new("mbr-two");
    let image = mbr_image(&[
        MbrPart::new(0x0C, 2, 8)
            .active()
            .holding(superblock(b"MSWIN4.1")),
        MbrPart::new(0x83, 16, 8).holding(superblock(b"        ")),
    ]);
    let path = tree.file("disk.img", &image);
    let Shape::Partitioned(table) = format::detect(&path).expect("detect") else {
        panic!("expected a partitioned disk");
    };
    assert_eq!(table.kind, part::TableKind::Mbr);
    assert_eq!(table.len(), 2);
    assert_eq!(table.get(1).map(|p| p.number), Some(1));
    assert_eq!(table.get(2).map(|p| p.type_label.as_str()), Some("Linux"));
    assert!(table.get(1).is_some_and(|p| p.bootable), "the active flag");
    assert!(!table.get(2).is_some_and(|p| p.bootable));
}

/// the own `ubuntu.iso` example, which every isohybrid image
/// would otherwise make unreachable.
#[test]
fn an_isohybrid_image_is_an_iso_and_not_a_partitioned_disk() {
    let tree = TempTree::new("isohybrid");
    // A protective MBR over bytes that are themselves an ISO 9660 volume.
    let mut image = mbr_image(&[MbrPart::new(0xEE, 1, 0xFFFF)]);
    image.resize(0x9000, 0);
    splice(&mut image, 0x8001, b"CD001");
    let path = tree.file("ubuntu.iso", &image);
    assert_eq!(
        format::detect(&path).expect("detect"),
        Shape::Volume(FsId::Iso9660),
        "the signature at offset 0 is read before the table"
    );
}

/// A file that is neither a filesystem nor a partitioned disk says so.
#[test]
fn a_file_that_is_neither_is_not_a_disk_image() {
    let tree = TempTree::new("neither");
    let path = tree.file("notes.img", &vec![0u8; 8192]);
    let err = format::detect(&path).expect_err("not an image");
    assert!(err.to_string().contains("not a disk image"), "{err}");
}

/// A protective MBR with no GPT behind it is **damaged**, not unpartitioned:
/// the disk says there should be a table.
#[test]
fn a_protective_mbr_with_no_gpt_is_damaged() {
    let tree = TempTree::new("protective");
    let mut image = mbr_image(&[MbrPart::new(0xEE, 1, 0xFFFF)]);
    image.resize(4096, 0);
    let path = tree.file("disk.img", &image);
    let err = format::detect(&path).expect_err("damaged");
    let text = err.to_string();
    assert!(text.contains("the partition table is damaged"), "{text}");
    assert!(
        !text.contains("not a disk image"),
        "a damaged table is not an unrecognised file: {text}"
    );
}

/// A GPT is read with its CRCs checked, and its labels and types survive.
#[test]
fn a_gpt_image_carries_its_labels_and_types() {
    let tree = TempTree::new("gpt");
    let esp = partitions::gpt::type_guids::EFI_SYSTEM;
    let linux = partitions::gpt::type_guids::LINUX_FILESYSTEM;
    let image = gpt_image(
        256,
        &[
            GptPart::new(esp, 34, 41)
                .named("EFI")
                .holding(superblock(b"MSWIN4.1")),
            GptPart::new(linux, 42, 49).named("root"),
        ],
    );
    let path = tree.file("disk.img", &image);
    let Shape::Partitioned(table) = format::detect(&path).expect("detect") else {
        panic!("expected a partitioned disk");
    };
    assert_eq!(table.kind, part::TableKind::Gpt);
    assert_eq!(table.len(), 2);
    let first = table.get(1).expect("partition 1");
    assert_eq!(first.label.as_deref(), Some("EFI"));
    assert_eq!(first.type_label, "EFI System");
    assert!(first.bootable, "an ESP is bootable by its type GUID");
    assert_eq!(first.region.len(), 8 * SECTOR as u64);
    let second = table.get(2).expect("partition 2");
    assert_eq!(second.type_label, "Linux filesystem");
    assert!(!second.bootable);
}

/// A partition whose window is not inside the file is refused **by number**,
/// with the reason, and its siblings still list (I3).
#[test]
fn a_partition_past_the_end_of_the_file_is_refused_by_number() {
    let tree = TempTree::new("past-end");
    let image = mbr_image(&[
        MbrPart::new(0x83, 2, 2).holding(superblock(b"        ")),
        // Declared at an LBA the file never reaches.
        MbrPart::new(0x83, 0x0010_0000, 64),
        MbrPart::new(0x83, 8, 2).holding(superblock(b"        ")),
    ]);
    let path = tree.file("truncated.img", &image);
    let Shape::Partitioned(table) = format::detect(&path).expect("detect") else {
        panic!("expected a partitioned disk");
    };
    assert_eq!(table.len(), 2, "the siblings still list");
    assert!(table.get(2).is_none(), "the refused one is not addressable");
    assert_eq!(table.get(3).map(|p| p.number), Some(3), "no renumbering");
    let why = table.refusal(2).expect("a reason for partition 2");
    assert!(why.contains("past the end of the image"), "{why}");
}

/// An extended partition is named and refused rather than shown as a
/// partition whose filesystem happens to be unrecognisable.
///
#[test]
fn an_extended_partition_is_refused_by_name() {
    let tree = TempTree::new("extended");
    let image = mbr_image(&[
        MbrPart::new(0x83, 2, 8).holding(superblock(b"        ")),
        // Declared with bytes that are really there, so what refuses it is
        // its type and not its window.
        MbrPart::new(0x05, 16, 8).holding(vec![0u8; 4096]),
    ]);
    let path = tree.file("disk.img", &image);
    let Shape::Partitioned(table) = format::detect(&path).expect("detect") else {
        panic!("expected a partitioned disk");
    };
    assert_eq!(table.len(), 1);
    let why = table.refusal(2).expect("a reason for partition 2");
    assert!(
        why.contains("an extended partition, not supported"),
        "{why}"
    );
}

/// The three states of the design stay apart: a recognised
/// filesystem with no reader is named, an unrecognised one says so, and
/// neither is the word for a damaged image (I13).
#[test]
fn an_unsupported_filesystem_is_named_rather_than_called_damaged() {
    assert_eq!(FsId::ExFat.label(), "exFAT");
    assert!(!FsId::ExFat.supported());
    assert!(FsId::ExFat.backend().is_none());
    let named = FsId::ExFat.refusal("partition 3").to_string();
    assert_eq!(named, "partition 3: exFAT, not supported");
    let ntfs = FsId::Ntfs.refusal("partition 1").to_string();
    assert_eq!(ntfs, "partition 1: NTFS, not supported");
    let unknown = FsId::Unknown.refusal("partition 2").to_string();
    assert_eq!(unknown, "partition 2: the filesystem was not recognised");
    for damaged in [&named, &ntfs, &unknown] {
        assert!(!damaged.contains("damaged"), "{damaged}");
    }

    // And the ones that gained a reader are on the other side of the same
    // question, which is the whole of what "supported" means here.
    for readable in [
        FsId::Iso9660,
        FsId::Fat12,
        FsId::Fat16,
        FsId::Fat32,
        FsId::Ext2,
        FsId::Ext3,
        FsId::Ext4,
        FsId::Squashfs,
    ] {
        assert!(readable.supported(), "{readable:?}");
        assert!(readable.backend().is_some(), "{readable:?}");
    }
    for refused in [FsId::Ntfs, FsId::HfsPlus, FsId::Apfs, FsId::LinuxSwap] {
        assert!(!refused.supported(), "{refused:?}");
        assert!(refused.backend().is_none(), "{refused:?}");
    }
}

/// A superblock is read for what it is, whatever the table's type byte says.
#[test]
fn each_partitions_superblock_is_sniffed() {
    let tree = TempTree::new("sniff");
    let image = mbr_image(&[
        // Type 0x83 says Linux; the bytes say exFAT. The bytes win, which is
        // what "detection is by content" means one level down.
        MbrPart::new(0x83, 2, 8).holding(superblock(b"EXFAT   ")),
        MbrPart::new(0x07, 16, 8).holding(superblock(b"NTFS    ")),
    ]);
    let path = tree.file("disk.img", &image);
    let Shape::Partitioned(table) = format::detect(&path).expect("detect") else {
        panic!("expected a partitioned disk");
    };
    assert_eq!(table.get(1).map(|p| p.fs), Some(FsId::ExFat));
    assert_eq!(table.get(2).map(|p| p.fs), Some(FsId::Ntfs));
    assert_eq!(
        table.get(2).map(|p| p.type_label.as_str()),
        Some("NTFS or exFAT")
    );
}

/// The extension is a hint, is consulted in one place, and never decides what
/// is inside (I14).
#[test]
fn the_extension_is_a_hint_and_only_a_hint() {
    for yes in ["a.iso", "a.img", "A.ISO", "backup.Img", "x.y.iso"] {
        assert!(format::looks_like_image_name(yes), "{yes}");
    }
    for no in ["a.isox", "iso", "a.tar.gz", "img", "", ".", "a."] {
        assert!(!format::looks_like_image_name(no), "{no}");
    }
}

// --------------------------------------------------------------------------
// The segment that addresses a partition
// --------------------------------------------------------------------------

/// A segment tail names a number and cannot smuggle a path in.
#[test]
fn a_partition_number_is_one_component_of_digits() {
    assert_eq!(
        part::partition_number(std::path::Path::new("/2")).ok(),
        Some(2)
    );
    assert_eq!(
        part::partition_number(std::path::Path::new("/128")).ok(),
        Some(128)
    );
    for bad in [
        "/2/3", "/a", "/", "/02x", "/-1", "/2 ", "//", "/129", "/0", "/1e0",
    ] {
        assert!(
            part::partition_number(std::path::Path::new(bad)).is_err(),
            "{bad} was accepted"
        );
    }
}

/// A partition row's real home is the filesystem's root, two `Image` segments
/// down, so `Enter` needs no change to `src/input/` or `src/panel/`.
///
#[test]
fn a_partition_row_addresses_the_volume_root() {
    let tree = TempTree::new("rows");
    let image = mbr_image(&[
        MbrPart::new(0x83, 2, 2).holding(superblock(b"        ")),
        MbrPart::new(0x83, 8, 2).holding(superblock(b"        ")),
    ]);
    let path = tree.file("backup.img", &image);
    let Shape::Partitioned(table) = format::detect(&path).expect("detect") else {
        panic!("expected a partitioned disk");
    };
    let root = image_root(&path);
    let rows = table.rows(&root);
    assert_eq!(rows.len(), 2);
    let second = rows.get(1).expect("the second row");
    assert_eq!(second.name, "2", "the name is the number and nothing else");
    assert_eq!(second.kind, EntryKind::Dir);
    let location = second.location.as_ref().expect("a real home");
    let kinds: Vec<BackendKind> = location.segments().iter().map(|(k, _)| *k).collect();
    assert_eq!(
        kinds,
        vec![BackendKind::Local, BackendKind::Image, BackendKind::Image]
    );
    assert_eq!(
        location.to_string(),
        format!("{}#/2#/", path.display()),
        "two hashes, because a partition is a segment"
    );
}

/// `VfsPath::parent` walks out of a volume, through the partition list, and
/// then out of the image (I16).
#[test]
fn leaving_a_volume_reaches_the_partition_list_and_then_the_directory() {
    let container = std::path::Path::new("/a/backup.img");
    let member = VfsPath::local(container)
        .with_segment(BackendKind::Image, "/2")
        .with_segment(BackendKind::Image, "/boot/vmlinuz");
    assert_eq!(member.to_string(), "/a/backup.img#/2#/boot/vmlinuz");

    let up = member.parent().expect("the directory holding it");
    assert_eq!(up.to_string(), "/a/backup.img#/2#/boot");
    let root = up.parent().expect("the volume root");
    assert_eq!(root.to_string(), "/a/backup.img#/2#/");
    let table = root.parent().expect("the partition list");
    assert_eq!(table.to_string(), "/a/backup.img#/");
    let outside = table.parent().expect("out of the image");
    assert_eq!(outside.to_string(), "/a");

    // One partition is not inside another, so a job in partition 2 is not a
    // job in partition 1.
    let one = VfsPath::local(container)
        .with_segment(BackendKind::Image, "/1")
        .with_segment(BackendKind::Image, "/");
    assert!(!member.starts_with(&one));
    assert!(
        member.starts_with(
            &VfsPath::local(container)
                .with_segment(BackendKind::Image, "/2")
                .with_segment(BackendKind::Image, "/")
        )
    );
}

// --------------------------------------------------------------------------
// The backend, its listing and its refusals
// --------------------------------------------------------------------------

/// The table view sends `..` first and unconditionally, then one row per
/// partition, then the reason for anything it would not address.
#[tokio::test]
async fn the_table_listing_sends_the_parent_row_first() {
    let tree = TempTree::new("listing");
    let image = mbr_image(&[
        MbrPart::new(0x83, 2, 2).holding(superblock(b"        ")),
        MbrPart::new(0x83, 0x0010_0000, 64),
    ]);
    let path = tree.file("backup.img", &image);
    let session = tree.session();
    let root = image_root(&path);
    let fs = session.open_image(&root).expect("open the image");
    assert_eq!(fs.view(), &ViewKind::Table);
    assert!(fs.filesystem().is_none());
    assert!(fs.table().is_some());

    // The `..` belongs to the read path now, so it is asked for, not awaited.
    let mut names: Vec<String> = fs.parent_row(&root).map(|p| p.name).into_iter().collect();
    let mut rx = fs.read_dir(&root);
    let mut errors = Vec::new();
    while let Some(item) = rx.recv().await {
        match item {
            Ok(entry) => names.push(entry.name),
            Err(err) => errors.push(err.to_string()),
        }
    }
    assert_eq!(names.first().map(String::as_str), Some(".."));
    assert_eq!(names, vec!["..".to_string(), "1".to_string()]);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors.first().is_some_and(|e| e.contains("partition 2")),
        "{errors:?}"
    );
}

/// A path typed into `Ctrl+G` that stops at the partition row is told where
/// the filesystem is rather than shown an empty panel.
///
#[tokio::test]
async fn a_partition_row_is_not_a_directory_in_the_tables_namespace() {
    let tree = TempTree::new("row-not-dir");
    let image = mbr_image(&[MbrPart::new(0x83, 2, 2).holding(superblock(b"        "))]);
    let path = tree.file("backup.img", &image);
    let session = tree.session();
    let fs = session.open_image(&image_root(&path)).expect("open");

    let row = VfsPath::local(&path).with_segment(BackendKind::Image, "/1");
    let mut rx = fs.read_dir(&row);
    let mut errors = Vec::new();
    while let Some(item) = rx.recv().await {
        if let Err(err) = item {
            errors.push(err.to_string());
        }
    }
    let first = errors.first().map(String::as_str).unwrap_or_default();
    assert!(first.contains("this is partition 1"), "{first}");
    assert!(first.contains("#/"), "{first}");

    // `stat` on the same row still answers, because it is a real row.
    let entry = Vfs::stat(fs.as_ref(), &row).expect("stat the row");
    assert_eq!(entry.name, "1");
    assert_eq!(entry.kind, EntryKind::Dir);
}

/// Every write path refuses **before** the container is opened, and
/// `Capabilities::writable` is false so the refusal reaches the UI before the
/// question (I7).
#[tokio::test]
async fn every_write_path_refuses_before_it_starts() {
    let tree = TempTree::new("read-only");
    let image = mbr_image(&[MbrPart::new(0x83, 2, 2).holding(superblock(b"        "))]);
    let path = tree.file("backup.img", &image);
    let session = tree.session();
    let fs = session.open_image(&image_root(&path)).expect("open");

    // A path that names nothing at all: the refusal must still be the
    // read-only one rather than an I/O error, which is what proves nothing was
    // opened on the way to it.
    let nowhere = VfsPath::local(&path).with_segment(BackendKind::Image, "/1/no/such/file");
    let refusals = [
        Vfs::open_write(fs.as_ref(), &nowhere).err(),
        Vfs::create_dir(fs.as_ref(), &nowhere).err(),
        Vfs::remove(fs.as_ref(), &nowhere).err(),
        Vfs::rename(fs.as_ref(), &nowhere, &nowhere).err(),
    ];
    for refusal in refusals {
        let text = refusal.expect("a refusal").to_string();
        assert!(text.contains("read-only"), "{text}");
        assert!(!text.contains("No such file"), "{text}");
    }
    assert!(!Vfs::capabilities(fs.as_ref()).writable);
    const { assert!(!crate::vfs::Capabilities::IMAGE.writable) };
    assert!(!BackendKind::Image.capabilities().writable);
    assert!(BackendKind::Image.capabilities().has_directories);
    assert_eq!(BackendKind::Image.id(), "image");
}

/// Two callers asking for the same image and the same partition get the same
/// instance, and therefore the same index (I15).
#[tokio::test]
async fn one_index_per_volume_and_one_per_partition() {
    let tree = TempTree::new("shared");
    let image = mbr_image(&[
        MbrPart::new(0x0C, 2, 8).holding(superblock(b"MSWIN4.1")),
        MbrPart::new(0x83, 16, 8).holding(superblock(b"        ")),
    ]);
    let path = tree.file("backup.img", &image);
    let session = tree.session();
    let root = image_root(&path);

    let one = session.open_image(&root).expect("open");
    let two = session.open_image(&root).expect("open again");
    assert!(Arc::ptr_eq(&one, &two), "one table view, shared");
    assert_eq!(session.open_image_count(), 1);

    let first = VfsPath::local(&path)
        .with_segment(BackendKind::Image, "/1")
        .with_segment(BackendKind::Image, "/");
    let second = VfsPath::local(&path)
        .with_segment(BackendKind::Image, "/2")
        .with_segment(BackendKind::Image, "/");
    let a = session.open_image(&first).expect("partition 1");
    let b = session.open_image(&first).expect("partition 1 again");
    let c = session.open_image(&second).expect("partition 2");
    assert!(Arc::ptr_eq(&a, &b), "one index per partition");
    assert!(!Arc::ptr_eq(&a, &c), "two partitions are two volumes");
    assert_eq!(a.partition(), Some(1));
    assert_eq!(c.partition(), Some(2));
    assert_eq!(session.open_image_count(), 3);

    let key = ArchiveSession::image_key_for(a.as_ref()).expect("a key");
    session.close_image(&key);
    assert_eq!(session.open_image_count(), 2);
}

/// An unsupported filesystem on one partition does not stop its siblings
/// listing, and it is reported by name (
/// the design).
#[tokio::test]
async fn an_unsupported_partition_reports_its_name_and_leaves_its_siblings_alone() {
    let tree = TempTree::new("exfat");
    let image = mbr_image(&[
        MbrPart::new(0x07, 2, 8).holding(superblock(b"EXFAT   ")),
        MbrPart::new(0x83, 16, 8).holding(superblock(b"        ")),
    ]);
    let path = tree.file("disk.img", &image);
    let session = tree.session();

    let exfat = VfsPath::local(&path)
        .with_segment(BackendKind::Image, "/1")
        .with_segment(BackendKind::Image, "/");
    let volume = session.open_image(&exfat).expect("open partition 1");
    assert_eq!(volume.filesystem(), Some(FsId::ExFat));
    let status = volume.wait_for_index();
    match status {
        crate::vfs::archive::index::IndexStatus::Failed(why) => {
            assert_eq!(why, "partition 1: exFAT, not supported");
        }
        other => panic!("expected a named refusal, got {other:?}"),
    }
    // Read-only whatever is on it, and no random access promised for a
    // filesystem nothing can read.
    let caps = Vfs::capabilities(volume.as_ref());
    assert!(!caps.writable);
    assert!(!caps.seekable);
    assert!(caps.has_directories);

    // The table still lists both, and the other one is untouched.
    let table = session
        .open_image(&image_root(&path))
        .expect("open the table");
    assert_eq!(table.table().map(part::Table::len), Some(2));
}

// --------------------------------------------------------------------------
// The container layer
// --------------------------------------------------------------------------

/// A reader cannot see a byte outside its own region, and a read that would
/// pass the end is short rather than leaking what follows (I4).
#[test]
fn a_region_cannot_read_past_its_own_end() {
    let tree = TempTree::new("region");
    let bytes: Vec<u8> = (0..=255u8).cycle().take(8192).collect();
    let path = tree.file("disk.bin", &bytes);

    let whole = block::Region::whole(&path).expect("whole");
    assert_eq!(whole.len(), 8192);
    assert!(!whole.is_empty());

    let window = whole.slice(4096, 1024).expect("a window");
    assert_eq!(window.start(), 4096);
    assert_eq!(window.len(), 1024);
    assert_eq!(window.container(), path.as_path());

    let mut reader = window.open().expect("open");
    let mut out = Vec::new();
    reader.read_to_end(&mut out).expect("read");
    assert_eq!(out.len(), 1024, "short at the region's end, never past it");
    assert_eq!(out.first().copied(), bytes.get(4096).copied());

    // Seeking past the end reads nothing rather than the neighbour's bytes.
    reader
        .seek(SeekFrom::Start(4096))
        .expect("seek past the end");
    let mut spill = [0u8; 16];
    assert_eq!(reader.read(&mut spill).expect("read"), 0);
    assert!(reader.seek(SeekFrom::Start(0)).is_ok());
    assert!(
        reader.seek(SeekFrom::Current(-1)).is_err(),
        "before the start"
    );

    // Bounds are saturating: an extent claiming everything answers `false`
    // rather than wrapping into `true`.
    assert!(window.contains(0, 1024));
    assert!(!window.contains(1, 1024));
    assert!(!window.contains(u64::MAX, u64::MAX));
    assert!(whole.slice(8000, 1024).is_err());
    assert!(whole.slice(0, 0).is_err());
    assert!(block::Region::sub(&path, 0, 8193).is_err());
}

/// The handle a filesystem library is given cannot write, and the file under
/// it is `O_RDONLY` so it could not honour a write if the refusal were removed
/// (I1).
#[test]
fn the_reader_refuses_every_write() {
    let tree = TempTree::new("no-write");
    let path = tree.file("disk.bin", &vec![7u8; 4096]);
    let mut reader = block::Region::whole(&path)
        .expect("whole")
        .open()
        .expect("open");
    let write = reader.write(b"x").expect_err("a refusal");
    assert_eq!(write.kind(), std::io::ErrorKind::PermissionDenied);
    let flush = reader.flush().expect_err("a refusal");
    assert_eq!(flush.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(std::fs::read(&path).expect("unchanged"), vec![7u8; 4096]);
}

/// Nothing reads an image whole: the sniff window is bounded by a constant
/// rather than by anything the image says (I6).
#[test]
fn the_sniff_window_is_bounded_by_a_constant() {
    let tree = TempTree::new("sniff-bound");
    let path = tree.file("big.bin", &vec![0u8; 128 * 1024]);
    let whole = block::Region::whole(&path).expect("whole");
    let window = block::sniff_window(&whole).expect("sniff");
    assert_eq!(window.len() as u64, block::SNIFF_LEN);
    const { assert!(block::SNIFF_LEN < 64 * 1024, "34 KiB, not the image") };

    // A region smaller than the window is short rather than padded.
    let small = tree.file("small.bin", &[0u8; 100]);
    let short = block::sniff_window(&block::Region::whole(&small).expect("whole")).expect("sniff");
    assert_eq!(short.len(), 100);
}

/// A partition's own bytes are copied and nothing else is (I4).
#[test]
fn a_copy_stays_inside_the_region_it_was_given() {
    let tree = TempTree::new("copy-range");
    let bytes: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
    let path = tree.file("disk.bin", &bytes);
    let whole = block::Region::whole(&path).expect("whole");
    let window = whole.slice(1024, 512).expect("window");

    let mut out = Vec::new();
    let written = block::copy_range(&window, 0, 512, &mut out).expect("copy");
    assert_eq!(written, 512);
    assert_eq!(out.as_slice(), bytes.get(1024..1536).unwrap_or_default());

    let mut spill = Vec::new();
    let err = block::copy_range(&window, 256, 512, &mut spill).expect_err("refused");
    assert!(err.to_string().contains("not inside this volume"), "{err}");
    assert!(spill.is_empty(), "nothing is written before the refusal");
}

/// An image with no partition table is the filesystem, and its first segment
/// is that filesystem's root.
#[tokio::test]
async fn an_unpartitioned_image_is_browsed_with_one_segment() {
    let tree = TempTree::new("one-segment");
    let path = tree.file(
        "card.img",
        &fat_image(2 * 1024 * 1024, &[("readme.txt", b"hello")]),
    );
    let session = tree.session();
    let root = image_root(&path);
    let fs = session.open_image(&root).expect("open");

    assert!(fs.table().is_none(), "no table, so no partition segment");
    assert_eq!(fs.partition(), None);
    assert!(matches!(fs.view(), ViewKind::Volume(_)));
    assert_eq!(fs.region().len(), 2 * 1024 * 1024);
    assert_eq!(fs.container(), path.as_path());
    assert!(!fs.is_cached(), "a local image is read where it lies");

    let mut names: Vec<String> = fs.parent_row(&root).map(|p| p.name).into_iter().collect();
    let mut rx = fs.read_dir(&root);
    while let Some(item) = rx.recv().await {
        if let Ok(entry) = item {
            names.push(entry.name);
        }
    }
    assert_eq!(names.first().map(String::as_str), Some(".."));
    assert!(names.iter().any(|n| n == "readme.txt"), "{names:?}");
    assert_eq!(root.to_string(), format!("{}#/", path.display()));
}

/// The session resolves the stack, and a path that is not inside an image is
/// refused rather than guessed at.
#[test]
fn a_path_that_is_not_inside_an_image_is_refused() {
    let tree = TempTree::new("not-an-image");
    let path = tree.file("card.img", &fat_image(2 * 1024 * 1024, &[("a.txt", b"x")]));
    let session = tree.session();

    let outside = VfsPath::local(&path);
    let err = session.open_image(&outside).expect_err("refused");
    assert!(err.to_string().contains("not inside a disk image"), "{err}");

    // And an image that is not there at all fails as an image, not as a panic.
    let missing =
        VfsPath::local(tree.root.join("nothing.img")).with_segment(BackendKind::Image, "/");
    assert!(session.open_image(&missing).is_err());
}

/// A partition describes itself in one line, for a status line and a message.
#[test]
fn a_partition_describes_itself_without_decorating_its_name() {
    let tree = TempTree::new("describe");
    let image = gpt_image(
        256,
        &[
            GptPart::new(partitions::gpt::type_guids::EFI_SYSTEM, 34, 97)
                .named("BOOT")
                .holding(superblock(b"MSWIN4.1")),
        ],
    );
    let path = tree.file("disk.img", &image);
    let Shape::Partitioned(table) = format::detect(&path).expect("detect") else {
        panic!("expected a partitioned disk");
    };
    let first = table.get(1).expect("partition 1");
    let text = first.describe();
    assert!(text.starts_with("partition 1, "), "{text}");
    assert!(text.contains("BOOT"), "{text}");
    assert!(text.ends_with("bootable"), "{text}");
    // The row's own name carries none of it (J13).
    assert_eq!(first.row(&image_root(&path)).name, "1");
}

/// A volume descriptor set with `junk` in the last four bytes of its
/// terminator, which is what a real mastering tool leaves there.
///
/// Sector 16 is a primary descriptor, sector 17 the terminator, sector 18
/// ordinary data that must come back untouched.
fn descriptor_set(junk: [u8; 4]) -> Vec<u8> {
    const SECTOR: usize = 2048;
    let mut image = vec![0u8; 19 * SECTOR];
    let mut header = |at: usize, ty: u8| {
        splice(&mut image, at, &[ty]);
        splice(&mut image, at + 1, b"CD001");
        splice(&mut image, at + 6, &[1]);
    };
    header(16 * SECTOR, 1);
    header(17 * SECTOR, 255);
    // The reserved body, which ECMA-119 says is zero and this image says is
    // not. Filled throughout, not only at the end, so the test would still
    // fail if the mask covered the last word alone.
    splice(&mut image, 17 * SECTOR + 7, &vec![0xEE; SECTOR - 7]);
    splice(&mut image, 18 * SECTOR - 4, &junk);
    splice(&mut image, 18 * SECTOR, &vec![0xAB; SECTOR]);
    image
}

#[test]
fn the_terminators_reserved_body_reads_as_the_standard_says_it_is() {
    const SECTOR: u64 = 2048;
    let tree = TempTree::new("term");
    let path = tree.file("junk.iso", &descriptor_set([0x83, 0xB4, 0x7C, 0xFA]));
    let region = super::block::Region::whole(&path).expect("region");

    let found = super::iso::Terminator::find(&region).expect("scan");
    assert_eq!(
        found,
        Some((17 * SECTOR + 7, 18 * SECTOR)),
        "the terminator"
    );

    let mut reader = super::iso::Terminator::over(&region).expect("open");
    let mut all = Vec::new();
    reader.read_to_end(&mut all).expect("read");

    // The header stays exactly as the image wrote it: this masks the reserved
    // body and nothing else.
    assert_eq!(all.get(17 * 2048), Some(&255u8), "the descriptor type");
    assert_eq!(all.get(17 * 2048 + 1..17 * 2048 + 6), Some(&b"CD001"[..]));
    // The body reads as zero, all of it, junk included.
    let body = all
        .get(17 * 2048 + 7..18 * 2048)
        .expect("the terminator body");
    assert!(body.iter().all(|b| *b == 0), "the reserved body is masked");
    // And the sector after it is untouched, so the mask is one sector wide.
    let after = all.get(18 * 2048..19 * 2048).expect("the sector after");
    assert!(after.iter().all(|b| *b == 0xAB), "only the body is masked");
}

#[test]
fn the_mask_holds_when_the_body_is_read_in_pieces() {
    let tree = TempTree::new("term-seek");
    let path = tree.file("junk.iso", &descriptor_set([0x83, 0xB4, 0x7C, 0xFA]));
    let region = super::block::Region::whole(&path).expect("region");
    let mut reader = super::iso::Terminator::over(&region).expect("open");

    // Straddling the mask's start: the tail of the identifier, the version
    // byte that ends the header at offset 6, and then the masked body.
    reader
        .seek(SeekFrom::Start(17 * 2048 + 3))
        .expect("seek into the header");
    let mut straddle = [0xFFu8; 8];
    reader.read_exact(&mut straddle).expect("read");
    assert_eq!(straddle.get(..3), Some(&b"001"[..]), "the header survives");
    assert_eq!(straddle.get(3), Some(&1u8), "the version byte survives");
    assert!(
        straddle.get(4..).is_some_and(|b| b.iter().all(|x| *x == 0)),
        "the body is masked from its first byte: {straddle:?}"
    );

    // And straddling its end, where the junk actually is.
    reader
        .seek(SeekFrom::Start(18 * 2048 - 6))
        .expect("seek to the junk");
    let mut tail = [0u8; 12];
    reader.read_exact(&mut tail).expect("read");
    assert!(
        tail.get(..6).is_some_and(|b| b.iter().all(|x| *x == 0)),
        "the junk reads as zero: {tail:?}"
    );
    assert!(
        tail.get(6..).is_some_and(|b| b.iter().all(|x| *x == 0xAB)),
        "the next sector is its own: {tail:?}"
    );
}
