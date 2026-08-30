//! The ext reader, against volumes built while the test runs.
//!
//! No image is checked in. `mke2fs` builds the fixture from a directory this
//! module writes, which is the same bargain the FAT tests strike with
//! `fatfs`'s own formatter: a volume written by a tool nobody here controls is
//! honest in a way a hand-rolled superblock is not, and `mke2fs` is the tool
//! that writes the ext volumes people actually browse.
//!
//! `mke2fs` is part of e2fsprogs and is present on every Linux distribution
//! and every GitHub runner. Where it is missing the test says so on stderr and
//! passes rather than failing for a reason that is not about this code - and
//! it says so loudly, because a check that quietly does nothing is worse than
//! no check.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::vfs::archive::index::{Builder, Index, IndexStatus};
use crate::vfs::archive::session::{ArchiveSession, RewriteLimits};
use crate::vfs::image::ImageFs;
use crate::vfs::{BackendKind, Vfs, VfsPath};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A throwaway directory, removed on drop.
struct TempTree {
    root: PathBuf,
}

impl TempTree {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("hcmd-ext-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp tree");
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Whether `mke2fs` is here, said out loud when it is not.
fn have_mke2fs(test: &str) -> bool {
    let found = std::process::Command::new("mke2fs")
        .arg("-V")
        .output()
        .is_ok();
    if !found {
        eprintln!(
            "SKIPPING {test}: mke2fs (e2fsprogs) is not installed, so no ext fixture could be built"
        );
    }
    found
}

/// The contents every fixture in this file holds.
///
/// A file at the root, a nested directory two deep, a file larger than one
/// block so the extent walk is exercised, a symbolic link, and a tar so that
/// an archive stored inside the volume can be read back out of it.
fn populate(tree: &Path) {
    std::fs::create_dir_all(tree.join("dir/sub")).expect("dirs");
    std::fs::write(tree.join("top.txt"), b"hello ext").expect("top");
    std::fs::write(tree.join("dir/big.bin"), big()).expect("big");
    std::fs::write(tree.join("dir/sub/deep.txt"), b"deep").expect("deep");
    std::os::unix::fs::symlink("dir/sub/deep.txt", tree.join("link")).expect("symlink");
    std::fs::write(tree.join("dir/inner.tar"), tarball()).expect("tar");
}

/// A file of 100 000 bytes, larger than any ext block size this test uses.
fn big() -> Vec<u8> {
    (0..100_000u32).map(|i| (i % 251) as u8).collect()
}

/// A `.tar` holding one file, built in process.
fn tarball() -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let body = b"inside the archive";
    let mut header = tar::Header::new_gnu();
    header.set_size(body.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(&mut header, "note.txt", &body[..])
        .expect("append");
    builder.into_inner().expect("tar bytes")
}

/// An ext volume of `blocks` 1 KiB blocks holding [`populate`]'s tree.
///
/// `None` when `mke2fs` is not installed. `-b 1024` keeps the fixture small;
/// `-d` is how e2fsprogs populates a volume without root and without mounting
/// anything.
fn ext_volume(tag: &str, kind: &str, blocks: u64) -> Option<Vec<u8>> {
    if !have_mke2fs(tag) {
        return None;
    }
    let tree = TempTree::new(tag);
    let source = tree.path("tree");
    std::fs::create_dir_all(&source).expect("source tree");
    populate(&source);
    let image = tree.path("volume.img");
    let status = std::process::Command::new("mke2fs")
        .args([
            "-q",
            "-F",
            "-t",
            kind,
            "-b",
            "1024",
            "-L",
            "HCMD TEST",
            "-d",
        ])
        .arg(&source)
        .arg(&image)
        .arg(blocks.to_string())
        .status()
        .expect("run mke2fs");
    assert!(status.success(), "mke2fs failed: {status}");
    Some(std::fs::read(&image).expect("read the volume back"))
}

/// Write `volume` into a container with `pad` zero bytes in front of it, and
/// return the region that addresses it.
///
/// `pad` is a partition simulated without a table: the volume does not begin
/// at byte 0, so a reader that forgets its region's start reads the padding.
fn at(tree: &TempTree, volume: &[u8], pad: usize) -> (PathBuf, Region) {
    let path = tree.path("container.img");
    let mut bytes = vec![0u8; pad];
    bytes.extend_from_slice(volume);
    std::fs::write(&path, &bytes).expect("container");
    let region = Region::sub(&path, pad as u64, volume.len() as u64).expect("region");
    (path, region)
}

/// Index `region` with the real sink, exactly as the backend does.
fn index_of(region: &Region) -> (Arc<Index>, Result<()>) {
    let index = Arc::new(Index::new());
    let outcome = {
        let mut sink = Builder::new(Arc::clone(&index), false);
        Ext.index(region, &mut sink)
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
    let mut names: Vec<String> = rows.into_iter().map(|row| row.name).collect();
    names.sort();
    names
}

/// Read one member back through the format, as `ImageFs::open_read` does.
fn read_member(region: &Region, index: &Index, path: &str) -> Vec<u8> {
    let member = index
        .get(path)
        .unwrap_or_else(|| panic!("{path} is indexed"));
    let mut out = Vec::new();
    let written = Ext.read_member(region, &member, &mut out).expect("read");
    assert_eq!(written, out.len() as u64, "{path} miscounted its bytes");
    out
}

/// The owner and group a file this test writes gets, which is what `mke2fs
/// -d` copies into the volume.
fn ids_of_this_user() -> (u32, u32) {
    use std::os::unix::fs::MetadataExt as _;
    let tree = TempTree::new("ids");
    let probe = tree.path("probe");
    std::fs::write(&probe, b"probe").expect("probe");
    let meta = std::fs::metadata(&probe).expect("metadata");
    (meta.uid(), meta.gid())
}

/// A 512-byte MBR with one partition of `sectors` sectors at `start_lba`.
fn mbr(start_lba: u32, sectors: u32) -> Vec<u8> {
    let mut sector = vec![0u8; 512];
    let entry = 446usize;
    if let Some(slot) = sector.get_mut(entry + 4..entry + 5) {
        // 0x83, the type byte every Linux filesystem partition carries.
        slot.copy_from_slice(&[0x83]);
    }
    if let Some(slot) = sector.get_mut(entry + 8..entry + 12) {
        slot.copy_from_slice(&start_lba.to_le_bytes());
    }
    if let Some(slot) = sector.get_mut(entry + 12..entry + 16) {
        slot.copy_from_slice(&sectors.to_le_bytes());
    }
    if let Some(slot) = sector.get_mut(510..512) {
        slot.copy_from_slice(&[0x55, 0xAA]);
    }
    sector
}

// --------------------------------------------------------------------------

/// Every directory of the volume lists, including a nested one, and `.` and
/// `..` never appear.
#[test]
fn an_ext4_volume_lists_every_directory() {
    let Some(volume) = ext_volume("list", "ext4", 4096) else {
        return;
    };
    let tree = TempTree::new("list-container");
    let (_path, region) = at(&tree, &volume, 0);
    let (index, outcome) = index_of(&region);
    outcome.expect("index");

    assert_eq!(
        names(&index, ""),
        vec![
            "dir".to_string(),
            "link".to_string(),
            "lost+found".to_string(),
            "top.txt".to_string()
        ],
        "mke2fs writes lost+found; . and .. are never listed"
    );
    assert_eq!(
        names(&index, "dir"),
        vec![
            "big.bin".to_string(),
            "inner.tar".to_string(),
            "sub".to_string()
        ]
    );
    assert_eq!(names(&index, "dir/sub"), vec!["deep.txt".to_string()]);
    assert!(index.is_dir("dir"));
    assert!(index.is_dir("dir/sub"));
    assert!(index.get(".").is_none());
    assert!(index.get("dir/..").is_none());
}

/// A member's bytes come back exactly, at both ends of the size range.
#[test]
fn a_member_reads_back_the_bytes_that_were_written() {
    let Some(volume) = ext_volume("read", "ext4", 4096) else {
        return;
    };
    let tree = TempTree::new("read-container");
    let (_path, region) = at(&tree, &volume, 0);
    let (index, _) = index_of(&region);

    assert_eq!(read_member(&region, &index, "top.txt"), b"hello ext");
    assert_eq!(read_member(&region, &index, "dir/sub/deep.txt"), b"deep");
    // Larger than one block, so this is the extent walk rather than one read.
    assert_eq!(read_member(&region, &index, "dir/big.bin"), big());
    assert_eq!(
        index.get("dir/big.bin").map(|m| m.size),
        Some(big().len() as u64)
    );
}

/// An archive stored inside the volume is a member like any other: its bytes
/// come back byte for byte, so it can be copied out and opened.
#[test]
fn an_archive_inside_the_volume_reads_back_exactly() {
    let Some(volume) = ext_volume("tar", "ext4", 4096) else {
        return;
    };
    let tree = TempTree::new("tar-container");
    let (_path, region) = at(&tree, &volume, 0);
    let (index, _) = index_of(&region);

    let bytes = read_member(&region, &index, "dir/inner.tar");
    assert_eq!(bytes, tarball());

    // And it really is an archive: the tar reader finds its one member.
    let mut archive = tar::Archive::new(std::io::Cursor::new(bytes));
    let mut found = Vec::new();
    for entry in archive.entries().expect("entries") {
        let mut entry = entry.expect("entry");
        let mut body = String::new();
        entry.read_to_string(&mut body).expect("body");
        found.push((entry.path().expect("path").display().to_string(), body));
    }
    assert_eq!(
        found,
        vec![("note.txt".to_string(), "inside the archive".to_string())]
    );
}

/// The mode, the owner and the group are reported because ext has them, and a
/// symbolic link is a symbolic link carrying its target.
#[test]
fn ext_reports_the_posix_metadata_it_has() {
    let Some(volume) = ext_volume("posix", "ext4", 4096) else {
        return;
    };
    let tree = TempTree::new("posix-container");
    let (_path, region) = at(&tree, &volume, 0);
    let (index, _) = index_of(&region);

    let top = index.get("top.txt").expect("top.txt");
    assert_ne!(top.mode, 0, "ext has mode bits and they are reported");
    assert_eq!(top.mode & 0o777, 0o644);
    // `mke2fs -d` copies the ownership of the files it was given, so the
    // owner in the image is whoever ran the test rather than a constant.
    let (uid, gid) = ids_of_this_user();
    assert_eq!(top.uid, uid, "the owner is read from the inode");
    assert_eq!(top.gid, gid);
    // 0.9.3 exposes no timestamp; the column is empty rather than invented.
    assert_eq!(top.mtime, None);

    let link = index.get("link").expect("link");
    match &link.kind {
        MemberKind::Symlink(target) => assert_eq!(target, "dir/sub/deep.txt"),
        other => panic!("the link listed as {other:?}"),
    }

    let caps = Ext.capabilities();
    assert!(!caps.writable, "a disk image is read-only");
    assert!(!caps.seekable, "an Ext4 handle cannot leave its call");
    assert!(!caps.random_access);
    assert!(!caps.can_execute);
    assert!(caps.has_directories);
}

/// A symbolic link is reported, never followed: reading one is refused rather
/// than answered with the bytes of the file it names.
#[test]
fn reading_a_symbolic_link_is_refused_rather_than_followed() {
    let Some(volume) = ext_volume("link", "ext4", 4096) else {
        return;
    };
    let tree = TempTree::new("link-container");
    let (_path, region) = at(&tree, &volume, 0);
    let (index, _) = index_of(&region);

    let link = index.get("link").expect("link");
    let mut out = Vec::new();
    let err = Ext
        .read_member(&region, &link, &mut out)
        .expect_err("refused");
    assert!(err.to_string().contains("symbolic link"), "{err}");
    assert!(out.is_empty(), "nothing was written before the refusal");
}

/// The volume label reaches the status line, and a directory is refused as a
/// thing to read.
#[test]
fn the_volume_label_and_the_directory_refusal() {
    let Some(volume) = ext_volume("label", "ext4", 4096) else {
        return;
    };
    let tree = TempTree::new("label-container");
    let (_path, region) = at(&tree, &volume, 0);
    assert_eq!(Ext.volume_label(&region).as_deref(), Some("HCMD TEST"));

    let (index, _) = index_of(&region);
    let dir = index.get("dir").expect("dir");
    let mut out = Vec::new();
    let err = Ext
        .read_member(&region, &dir, &mut out)
        .expect_err("refused");
    assert!(err.to_string().contains("directory"), "{err}");
}

/// ext2 is the same reader and the same answers.
#[test]
fn an_ext2_volume_reads_with_the_same_reader() {
    let Some(volume) = ext_volume("ext2", "ext2", 4096) else {
        return;
    };
    let tree = TempTree::new("ext2-container");
    let (_path, region) = at(&tree, &volume, 0);
    let (index, outcome) = index_of(&region);
    outcome.expect("index");
    assert_eq!(read_member(&region, &index, "top.txt"), b"hello ext");
    assert_eq!(names(&index, "dir/sub"), vec!["deep.txt".to_string()]);
}

/// A volume that does not begin at byte 0 is read from where it begins, and
/// nothing outside its window is reachable.
#[test]
fn a_volume_inside_a_region_reads_only_that_region() {
    let Some(volume) = ext_volume("offset", "ext4", 4096) else {
        return;
    };
    let tree = TempTree::new("offset-container");
    let (_path, region) = at(&tree, &volume, 1024 * 1024);
    let (index, outcome) = index_of(&region);
    outcome.expect("index");
    assert_eq!(read_member(&region, &index, "top.txt"), b"hello ext");
}

/// A file inside a partition inside an image, through the whole stack: the
/// table is read, the partition is a segment, and the member's bytes come out
/// of `Vfs::open_read`.
#[test]
fn a_file_inside_a_partition_inside_an_image() {
    let Some(volume) = ext_volume("partition", "ext4", 4096) else {
        return;
    };
    let tree = TempTree::new("partition-container");
    // The volume at LBA 2048, which is where every partitioning tool starts
    // the first partition.
    let start = 2048usize;
    let mut image = mbr(
        start as u32,
        u32::try_from(volume.len() / 512).expect("sector count"),
    );
    image.resize(start * 512, 0);
    image.extend_from_slice(&volume);
    let path = tree.path("disk.img");
    std::fs::write(&path, &image).expect("image");

    let format::Shape::Partitioned(table) = format::detect(&path).expect("detect") else {
        panic!("expected a partitioned disk");
    };
    assert_eq!(table.len(), 1);
    assert!(
        matches!(
            table.get(1).map(|p| p.fs),
            Some(FsId::Ext2 | FsId::Ext3 | FsId::Ext4)
        ),
        "the sniffer named {:?}",
        table.get(1).map(|p| p.fs)
    );

    let session = ArchiveSession::in_dir(&tree.root, RewriteLimits::default()).expect("session");
    let display = VfsPath::local(&path).with_segment(BackendKind::Image, "/");
    let fs = ImageFs::open(&session, &path, display.clone(), Some(1)).expect("partition 1");
    assert_eq!(fs.wait_for_index(), IndexStatus::Complete);

    let member = VfsPath::local(&path)
        .with_segment(BackendKind::Image, "/1")
        .with_segment(BackendKind::Image, "/dir/sub/deep.txt");
    let entry = Vfs::stat(&fs, &member).expect("stat");
    assert_eq!(entry.name, "deep.txt");
    assert_eq!(entry.size, 4);

    let mut body = Vec::new();
    Vfs::open_read(&fs, &member)
        .expect("open")
        .read_to_end(&mut body)
        .expect("read");
    assert_eq!(body, b"deep");

    // A symbolic link is answered by `read_link`, judged on the way out, and
    // never followed by this backend.
    let link = VfsPath::local(&path)
        .with_segment(BackendKind::Image, "/1")
        .with_segment(BackendKind::Image, "/link");
    assert_eq!(
        Vfs::read_link(&fs, &link).expect("read_link"),
        "dir/sub/deep.txt"
    );

    // An archive inside the image is still refused where it always was, with
    // the message that says what to do instead: this reader does not change
    // that answer, it only makes the archive visible.
    let inner = VfsPath::local(&path)
        .with_segment(BackendKind::Image, "/1")
        .with_segment(BackendKind::Image, "/dir/inner.tar")
        .with_segment(BackendKind::Archive, "/");
    let err = session.open(&inner).expect_err("refused");
    assert!(err.to_string().contains("copy it out first"), "{err}");
}

/// A volume whose bytes stop halfway is reported, not panicked over, and the
/// message is about the image rather than about a bug.
#[test]
fn a_truncated_volume_is_reported() {
    let Some(volume) = ext_volume("truncated", "ext4", 4096) else {
        return;
    };
    let tree = TempTree::new("truncated-container");
    // Enough for the superblock and the descriptors, far short of the data.
    let cut = volume.len() / 8;
    let (_path, region) = at(&tree, volume.get(..cut).unwrap_or_default(), 0);
    let (index, outcome) = index_of(&region);
    let err = outcome.expect_err("a truncated volume cannot be walked");
    let text = err.to_string();
    assert!(!text.is_empty());
    assert!(
        matches!(index.status(), IndexStatus::Failed(_)),
        "{:?}",
        index.status()
    );
}

/// A superblock full of the wrong bytes is damage, reported by name, and
/// nothing panics.
#[test]
fn a_corrupt_superblock_is_reported_as_damage() {
    let Some(volume) = ext_volume("corrupt", "ext4", 4096) else {
        return;
    };
    let mut broken = volume.clone();
    // The superblock lives at 1024 and its magic is 0xEF53 at offset 56 of it.
    for offset in 1024..2048 {
        if let Some(byte) = broken.get_mut(offset) {
            *byte = 0xA5;
        }
    }
    let tree = TempTree::new("corrupt-container");
    let (_path, region) = at(&tree, &broken, 0);
    let (_index, outcome) = index_of(&region);
    let err = outcome.expect_err("a corrupt superblock cannot be opened");
    assert!(err.to_string().contains("damaged"), "{err}");

    // And the label asks the same damaged volume a question and answers None
    // rather than failing.
    assert_eq!(Ext.volume_label(&region), None);
}

/// A volume of no ext at all is refused as damage rather than read as an empty
/// one.
#[test]
fn zeroes_are_not_an_ext_volume() {
    let tree = TempTree::new("zeroes");
    let (_path, region) = at(&tree, &vec![0u8; 64 * 1024], 0);
    let (_index, outcome) = index_of(&region);
    let err = outcome.expect_err("zeroes are not a filesystem");
    assert!(err.to_string().contains("damaged"), "{err}");
}
