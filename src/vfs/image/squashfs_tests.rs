//! The SquashFS reader, against volumes this file builds byte by byte.
//!
//! `mksquashfs` is not the fixture here, and that is a decision rather than an
//! accident: squashfs-tools is not installed on a stock developer machine and
//! is not on a GitHub runner either, so a suite that needed it would be a
//! suite that never ran. A SquashFS with every block stored uncompressed is a
//! superblock, two metadata blocks and the file data, which is small enough to
//! write here and readable enough to argue with - the same bargain the MBR and
//! GPT fixtures in `image/tests.rs` strike.
//!
//! What the builder does *not* do is compress, so the decompressors are not
//! exercised. They are `squashfs_reader`'s own and are tested there; what is
//! tested here is this crate's reader - the walk, the names, the metadata, the
//! bounds and the refusals.

use std::path::PathBuf;
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
        let root = std::env::temp_dir().join(format!("hcmd-sqfs-{tag}-{}-{n}", std::process::id()));
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

// --------------------------------------------------------------------------
// The fixture builder
// --------------------------------------------------------------------------

/// The data block size every fixture uses, and its log2.
const BLOCK: usize = 4096;
const BLOCK_LOG: u16 = 12;

/// The bit a block size carries when the block is stored uncompressed.
const UNCOMPRESSED: u32 = 1 << 24;

/// The bit a metadata block's length carries when it is stored uncompressed.
const METADATA_UNCOMPRESSED: u16 = 0x8000;

/// The uid and the gid every fixture's id table holds, in that order.
const IDS: [u32; 2] = [1000, 100];

/// The modification time every inode carries.
const MTIME: u32 = 1_700_000_000;

/// One entry of the tree a fixture holds.
enum Item {
    /// A regular file and its bytes.
    File(&'static str, Vec<u8>),
    /// A symbolic link and its target, exactly as the image stores it.
    Link(&'static str, &'static str),
    /// A directory and what is in it.
    Dir(&'static str, Vec<Item>),
    /// A device node: an inode type `squashfs_reader` 0.1.1 does not model
    /// and answers `unimplemented!()` for. Firmware images are full of them.
    Device(&'static str),
}

/// What a node became once the layout was decided.
struct Node {
    name: String,
    kind: Kind,
    inode_number: u32,
    /// Where this inode starts inside the one inode metadata block.
    inode_offset: u16,
    /// Where this directory's listing starts inside the one directory
    /// metadata block, and how many bytes it is.
    listing_offset: u16,
    listing_len: u32,
    /// Where a file's data starts in the archive, and its block sizes.
    data_start: u64,
    blocks: Vec<u32>,
}

enum Kind {
    File(Vec<u8>),
    Link(String),
    Dir(Vec<usize>),
    Device,
}

impl Kind {
    /// The type tag a directory entry carries for this kind.
    fn tag(&self) -> u16 {
        match self {
            Self::Dir(_) => 1,
            Self::File(_) => 2,
            Self::Link(_) => 3,
            // A block device, which is what the reader must not fall over.
            Self::Device => 4,
        }
    }
}

/// Flatten `items` into an arena, the root first, children after their parent.
fn arena(items: Vec<Item>) -> Vec<Node> {
    let mut nodes = vec![Node {
        name: String::new(),
        kind: Kind::Dir(Vec::new()),
        inode_number: 1,
        inode_offset: 0,
        listing_offset: 0,
        listing_len: 0,
        data_start: 0,
        blocks: Vec::new(),
    }];
    add_children(&mut nodes, 0, items);
    nodes
}

/// Add `items` to `parent`, recursively.
fn add_children(nodes: &mut Vec<Node>, parent: usize, items: Vec<Item>) {
    let mut items = items;
    items.sort_by(|a, b| name_of(a).cmp(name_of(b)));
    for item in items {
        let index = nodes.len();
        let number = u32::try_from(index + 1).expect("inode number");
        let (name, kind) = match item {
            Item::File(name, bytes) => (name.to_string(), Kind::File(bytes)),
            Item::Link(name, target) => (name.to_string(), Kind::Link(target.to_string())),
            Item::Device(name) => (name.to_string(), Kind::Device),
            Item::Dir(name, children) => {
                nodes.push(Node {
                    name: name.to_string(),
                    kind: Kind::Dir(Vec::new()),
                    inode_number: number,
                    inode_offset: 0,
                    listing_offset: 0,
                    listing_len: 0,
                    data_start: 0,
                    blocks: Vec::new(),
                });
                if let Some(Kind::Dir(kids)) = nodes.get_mut(parent).map(|n| &mut n.kind) {
                    kids.push(index);
                }
                add_children(nodes, index, children);
                continue;
            }
        };
        nodes.push(Node {
            name,
            kind,
            inode_number: number,
            inode_offset: 0,
            listing_offset: 0,
            listing_len: 0,
            data_start: 0,
            blocks: Vec::new(),
        });
        if let Some(Kind::Dir(kids)) = nodes.get_mut(parent).map(|n| &mut n.kind) {
            kids.push(index);
        }
    }
}

fn name_of(item: &Item) -> &str {
    match item {
        Item::File(name, _) | Item::Link(name, _) | Item::Dir(name, _) | Item::Device(name) => name,
    }
}

/// A SquashFS 4.0 image holding `items`, every block stored uncompressed.
///
/// The layout, in order: the 96-byte superblock, the file data, the one
/// metadata block holding every inode, the one metadata block holding every
/// directory listing, the id table's metadata block, and the id table's index.
fn squashfs(items: Vec<Item>) -> Vec<u8> {
    let mut nodes = arena(items);
    let mut data = Vec::new();

    // The file data, and the block size list each file's inode carries.
    for node in nodes.iter_mut() {
        let Kind::File(bytes) = &node.kind else {
            continue;
        };
        node.data_start = (96 + data.len()) as u64;
        for chunk in bytes.chunks(BLOCK) {
            node.blocks
                .push(u32::try_from(chunk.len()).expect("block size") | UNCOMPRESSED);
            data.extend_from_slice(chunk);
        }
    }

    // Where each inode will land inside the inode block. Assigned before the
    // listings are built, because a directory entry names an inode by its
    // offset, and used again when the inodes are written.
    let mut offset = 0usize;
    for index in 0..nodes.len() {
        let Some(node) = nodes.get_mut(index) else {
            continue;
        };
        node.inode_offset = u16::try_from(offset).expect("inode offset");
        offset = offset.saturating_add(inode_len(node));
    }
    assert!(offset <= 8192, "the fixture's inodes must fit one block");

    // The directory listings, in the same order.
    let mut listings = Vec::new();
    for index in 0..nodes.len() {
        let children = match nodes.get(index).map(|n| &n.kind) {
            Some(Kind::Dir(kids)) => kids.clone(),
            _ => continue,
        };
        let start = listings.len();
        if !children.is_empty() {
            let base = nodes
                .get(*children.first().unwrap_or(&0))
                .map(|n| n.inode_number)
                .unwrap_or(1);
            listings.extend_from_slice(&u32::try_from(children.len() - 1).unwrap().to_le_bytes());
            listings.extend_from_slice(&0u32.to_le_bytes()); // the inode block
            listings.extend_from_slice(&base.to_le_bytes());
            for child in &children {
                let Some(node) = nodes.get(*child) else {
                    continue;
                };
                let delta = i32::try_from(node.inode_number).unwrap_or(0)
                    - i32::try_from(base).unwrap_or(0);
                listings.extend_from_slice(&node.inode_offset.to_le_bytes());
                listings
                    .extend_from_slice(&i16::try_from(delta).expect("inode delta").to_le_bytes());
                listings.extend_from_slice(&node.kind.tag().to_le_bytes());
                let name = node.name.as_bytes();
                listings.extend_from_slice(
                    &u16::try_from(name.len() - 1)
                        .expect("name size")
                        .to_le_bytes(),
                );
                listings.extend_from_slice(name);
            }
        }
        if let Some(node) = nodes.get_mut(index) {
            node.listing_offset = u16::try_from(start).expect("listing offset");
            node.listing_len = u32::try_from(listings.len() - start).expect("listing length");
        }
    }
    assert!(
        listings.len() <= 8192,
        "the fixture's listings must fit one block"
    );

    // The inodes themselves.
    let mut inodes = Vec::new();
    for node in &nodes {
        inodes.extend_from_slice(&node.kind.tag().to_le_bytes());
        inodes.extend_from_slice(&permissions(&node.kind).to_le_bytes());
        inodes.extend_from_slice(&0u16.to_le_bytes()); // uid index
        inodes.extend_from_slice(&1u16.to_le_bytes()); // gid index
        inodes.extend_from_slice(&MTIME.to_le_bytes());
        inodes.extend_from_slice(&node.inode_number.to_le_bytes());
        match &node.kind {
            Kind::Dir(_) | Kind::Device => {
                inodes.extend_from_slice(&0u32.to_le_bytes()); // listing block
                inodes.extend_from_slice(&1u32.to_le_bytes()); // link count
                // A directory's declared size is its listing plus the three
                // bytes the kernel's `.` and `..` occupy.
                inodes.extend_from_slice(
                    &u16::try_from(node.listing_len + 3)
                        .expect("listing size")
                        .to_le_bytes(),
                );
                inodes.extend_from_slice(&node.listing_offset.to_le_bytes());
                inodes.extend_from_slice(&1u32.to_le_bytes()); // parent inode
            }
            Kind::File(bytes) => {
                inodes.extend_from_slice(
                    &u32::try_from(node.data_start)
                        .expect("data start")
                        .to_le_bytes(),
                );
                inodes.extend_from_slice(&u32::MAX.to_le_bytes()); // no fragment
                inodes.extend_from_slice(&0u32.to_le_bytes()); // fragment offset
                inodes.extend_from_slice(
                    &u32::try_from(bytes.len()).expect("file size").to_le_bytes(),
                );
                for size in &node.blocks {
                    inodes.extend_from_slice(&size.to_le_bytes());
                }
            }
            Kind::Link(target) => {
                inodes.extend_from_slice(&1u32.to_le_bytes()); // link count
                inodes.extend_from_slice(
                    &u32::try_from(target.len())
                        .expect("target size")
                        .to_le_bytes(),
                );
                inodes.extend_from_slice(target.as_bytes());
            }
        }
    }

    let mut image = vec![0u8; 96];
    image.extend_from_slice(&data);
    let inode_table = image.len() as u64;
    image.extend_from_slice(&metadata_block(&inodes));
    let dir_table = image.len() as u64;
    image.extend_from_slice(&metadata_block(&listings));
    let id_block = image.len() as u64;
    let ids: Vec<u8> = IDS.iter().flat_map(|id| id.to_le_bytes()).collect();
    image.extend_from_slice(&metadata_block(&ids));
    let id_table = image.len() as u64;
    image.extend_from_slice(&id_block.to_le_bytes());

    let root_inode = u64::from(nodes.first().map(|n| n.inode_offset).unwrap_or(0));
    let bytes_used = image.len() as u64;
    let mut header = Vec::new();
    header.extend_from_slice(b"hsqs");
    header.extend_from_slice(
        &u32::try_from(nodes.len())
            .expect("inode count")
            .to_le_bytes(),
    );
    header.extend_from_slice(&MTIME.to_le_bytes());
    header.extend_from_slice(&u32::try_from(BLOCK).expect("block size").to_le_bytes());
    header.extend_from_slice(&0u32.to_le_bytes()); // no fragments
    header.extend_from_slice(&1u16.to_le_bytes()); // the gzip compressor id
    header.extend_from_slice(&BLOCK_LOG.to_le_bytes());
    // Inodes, data, fragments and the id table are all stored uncompressed,
    // and there are no fragments at all.
    header.extend_from_slice(&0x091Bu16.to_le_bytes());
    header.extend_from_slice(&u16::try_from(IDS.len()).expect("id count").to_le_bytes());
    header.extend_from_slice(&4u16.to_le_bytes()); // version major
    header.extend_from_slice(&0u16.to_le_bytes()); // version minor
    header.extend_from_slice(&root_inode.to_le_bytes());
    header.extend_from_slice(&bytes_used.to_le_bytes());
    header.extend_from_slice(&id_table.to_le_bytes());
    header.extend_from_slice(&u64::MAX.to_le_bytes()); // no xattrs
    header.extend_from_slice(&inode_table.to_le_bytes());
    header.extend_from_slice(&dir_table.to_le_bytes());
    header.extend_from_slice(&id_table.to_le_bytes()); // no fragment table
    header.extend_from_slice(&u64::MAX.to_le_bytes()); // no export table
    assert_eq!(header.len(), 96);
    if let Some(slot) = image.get_mut(..96) {
        slot.copy_from_slice(&header);
    }
    image
}

/// How many bytes one inode takes on disk.
fn inode_len(node: &Node) -> usize {
    match &node.kind {
        Kind::Dir(_) | Kind::Device => 32,
        Kind::File(_) => 32usize.saturating_add(node.blocks.len().saturating_mul(4)),
        Kind::Link(target) => 24usize.saturating_add(target.len()),
    }
}

/// The mode bits a kind carries, so that a test can tell them apart.
fn permissions(kind: &Kind) -> u16 {
    match kind {
        Kind::Dir(_) => 0o755,
        Kind::File(_) => 0o644,
        Kind::Link(_) => 0o777,
        Kind::Device => 0o600,
    }
}

/// One metadata block: a 16-bit length with the uncompressed bit set, then the
/// bytes.
fn metadata_block(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let len = u16::try_from(body.len()).expect("metadata block length");
    out.extend_from_slice(&(len | METADATA_UNCOMPRESSED).to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// A file of 10 000 bytes, which is three data blocks.
fn big() -> Vec<u8> {
    (0..10_000u32).map(|i| (i % 251) as u8).collect()
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

/// The tree every ordinary fixture in this file holds.
fn tree() -> Vec<Item> {
    vec![
        Item::File("top.txt", b"hello squashfs".to_vec()),
        Item::Dir(
            "dir",
            vec![
                Item::File("big.bin", big()),
                Item::File("inner.tar", tarball()),
                Item::Dir("sub", vec![Item::File("deep.txt", b"deep".to_vec())]),
            ],
        ),
    ]
}

/// Write `volume` into a container with `pad` zero bytes in front of it.
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
        Squashfs.index(region, &mut sink)
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

fn read_member(region: &Region, index: &Index, path: &str) -> Vec<u8> {
    let member = index
        .get(path)
        .unwrap_or_else(|| panic!("{path} is indexed"));
    let mut out = Vec::new();
    let written = Squashfs
        .read_member(region, &member, &mut out)
        .expect("read");
    assert_eq!(written, out.len() as u64, "{path} miscounted its bytes");
    out
}

/// A 512-byte MBR with one partition of `sectors` sectors at `start_lba`.
fn mbr(start_lba: u32, sectors: u32) -> Vec<u8> {
    let mut sector = vec![0u8; 512];
    let entry = 446usize;
    if let Some(slot) = sector.get_mut(entry + 4..entry + 5) {
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

/// The fixture builder is a fixture: if it stopped producing a SquashFS this
/// whole file would be testing nothing, so the first test is that the sniffer
/// agrees what it built.
#[test]
fn the_fixture_is_a_squashfs_by_content() {
    let tree = TempTree::new("sniff");
    let (path, region) = at(&tree, &squashfs(self::tree()), 0);
    assert_eq!(format::sniff(&region).expect("sniff"), FsId::Squashfs);
    assert_eq!(
        format::detect(&path).expect("detect"),
        format::Shape::Volume(FsId::Squashfs)
    );
    assert!(FsId::Squashfs.supported());
    assert!(FsId::Squashfs.backend().is_some());
}

/// Every directory lists, including a nested one.
#[test]
fn a_squashfs_volume_lists_every_directory() {
    let tree = TempTree::new("list");
    let (_path, region) = at(&tree, &squashfs(self::tree()), 0);
    let (index, outcome) = index_of(&region);
    outcome.expect("index");
    assert_eq!(index.status(), IndexStatus::Complete);

    assert_eq!(
        names(&index, ""),
        vec!["dir".to_string(), "top.txt".to_string()]
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
}

/// A member's bytes come back exactly, across more than one data block.
#[test]
fn a_member_reads_back_the_bytes_that_were_written() {
    let tree = TempTree::new("read");
    let (_path, region) = at(&tree, &squashfs(self::tree()), 0);
    let (index, _) = index_of(&region);

    assert_eq!(read_member(&region, &index, "top.txt"), b"hello squashfs");
    assert_eq!(read_member(&region, &index, "dir/sub/deep.txt"), b"deep");
    assert_eq!(read_member(&region, &index, "dir/big.bin"), big());
    assert_eq!(
        index.get("dir/big.bin").map(|m| m.size),
        Some(big().len() as u64)
    );
}

/// An archive stored inside the volume reads back byte for byte.
#[test]
fn an_archive_inside_the_volume_reads_back_exactly() {
    let tree = TempTree::new("tar");
    let (_path, region) = at(&tree, &squashfs(self::tree()), 0);
    let (index, _) = index_of(&region);
    let bytes = read_member(&region, &index, "dir/inner.tar");
    assert_eq!(bytes, tarball());

    let mut archive = tar::Archive::new(std::io::Cursor::new(bytes));
    let count = archive.entries().expect("entries").count();
    assert_eq!(count, 1, "the tar is still a tar");
}

/// The mode, the owner, the group, the time and a symbolic link's target are
/// all real fields of the inode and are all reported.
#[test]
fn squashfs_reports_the_posix_metadata_it_has() {
    let tree = TempTree::new("posix");
    let (_path, region) = at(&tree, &squashfs(self::tree()), 0);
    let (index, _) = index_of(&region);

    let top = index.get("top.txt").expect("top.txt");
    assert_eq!(top.mode & 0o7777, 0o644);
    assert_eq!(top.uid, IDS[0], "the uid comes from the id table");
    assert_eq!(top.gid, IDS[1], "and the gid is the other entry");
    assert_eq!(
        top.mtime,
        Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(u64::from(MTIME)))
    );

    let dir = index.get("dir").expect("dir");
    assert_eq!(dir.mode & 0o7777, 0o755);
    assert_eq!(dir.size, 0, "a directory's listing size is not a file size");

    let caps = Squashfs.capabilities();
    assert!(!caps.writable, "a disk image is read-only");
    assert!(caps.seekable, "a FileReader owns its volume");
    assert!(caps.random_access);
    assert!(!caps.can_execute);
    assert!(caps.has_directories);
}

/// A member can be seeked, which is what the viewer opens a large file with.
#[test]
fn a_member_can_be_opened_and_seeked() {
    use std::io::{Read as _, Seek as _, SeekFrom};

    let tree = TempTree::new("seek");
    let (_path, region) = at(&tree, &squashfs(self::tree()), 0);
    let (index, _) = index_of(&region);

    let member = index.get("dir/big.bin").expect("big.bin");
    let mut handle = Squashfs
        .open_member(&region, &member)
        .expect("open")
        .expect("a seekable handle");
    handle.seek(SeekFrom::Start(9_000)).expect("seek");
    let mut tail = Vec::new();
    handle.read_to_end(&mut tail).expect("read");
    assert_eq!(tail, big().get(9_000..).expect("the tail").to_vec());

    // A directory is not something to hand a reader out for.
    let dir = index.get("dir").expect("dir");
    assert!(
        Squashfs
            .open_member(&region, &dir)
            .expect("asked")
            .is_none(),
        "a directory has no handle"
    );
}

/// A symbolic link is one entry lost, not one image lost.
///
/// `squashfs_reader` 0.1.1 panics as it reads a directory entry that is not a
/// file or a directory, so a symbolic link cannot be listed at all. What this
/// pins down is the shape of the loss: the sibling rows are all there, the
/// nested directory under the link is still walked, and the count comes back
/// as an error the panel shows under the listing.
#[test]
fn a_symbolic_link_is_skipped_and_counted_rather_than_losing_the_image() {
    let tree = TempTree::new("link");
    let mut items = self::tree();
    items.push(Item::Link("link", "dir/sub/deep.txt"));
    let (_path, region) = at(&tree, &squashfs(items), 0);
    let (index, outcome) = index_of(&region);

    let err = outcome.expect_err("the skipped entry is reported");
    let text = err.to_string();
    assert!(text.contains("1 entry was not listed"), "{text}");
    assert!(text.contains("symbolic link"), "{text}");

    assert_eq!(
        names(&index, ""),
        vec!["dir".to_string(), "top.txt".to_string()],
        "everything but the link is still listed"
    );
    assert_eq!(names(&index, "dir/sub"), vec!["deep.txt".to_string()]);
    assert_eq!(read_member(&region, &index, "top.txt"), b"hello squashfs");
    assert!(index.get("link").is_none(), "and the link is not invented");
}

/// A volume that does not begin at byte 0 is read from where it begins.
#[test]
fn a_volume_inside_a_region_reads_only_that_region() {
    let tree = TempTree::new("offset");
    let (_path, region) = at(&tree, &squashfs(self::tree()), 1024 * 1024);
    let (index, outcome) = index_of(&region);
    outcome.expect("index");
    assert_eq!(read_member(&region, &index, "top.txt"), b"hello squashfs");
}

/// A file inside a partition inside an image, through the whole stack.
#[test]
fn a_file_inside_a_partition_inside_an_image() {
    let tree = TempTree::new("partition");
    let volume = squashfs(self::tree());
    let start = 2048usize;
    let sectors = u32::try_from(volume.len().div_ceil(512)).expect("sector count");
    let mut image = mbr(start as u32, sectors);
    image.resize(start * 512, 0);
    image.extend_from_slice(&volume);
    image.resize(start * 512 + (sectors as usize) * 512, 0);
    let path = tree.path("disk.img");
    std::fs::write(&path, &image).expect("image");

    let format::Shape::Partitioned(table) = format::detect(&path).expect("detect") else {
        panic!("expected a partitioned disk");
    };
    assert_eq!(table.get(1).map(|p| p.fs), Some(FsId::Squashfs));

    let session = ArchiveSession::in_dir(&tree.root, RewriteLimits::default()).expect("session");
    let display = VfsPath::local(&path).with_segment(BackendKind::Image, "/");
    let fs = ImageFs::open(&session, &path, display, Some(1)).expect("partition 1");
    assert_eq!(fs.wait_for_index(), IndexStatus::Complete);

    let member = VfsPath::local(&path)
        .with_segment(BackendKind::Image, "/1")
        .with_segment(BackendKind::Image, "/dir/sub/deep.txt");
    let entry = Vfs::stat(&fs, &member).expect("stat");
    assert_eq!(entry.name, "deep.txt");
    assert_eq!(entry.size, 4);

    let mut body = Vec::new();
    std::io::Read::read_to_end(&mut Vfs::open_read(&fs, &member).expect("open"), &mut body)
        .expect("read");
    assert_eq!(body, b"deep");
}

/// An inode type the library does not model is a refusal, not a dead thread.
///
/// `squashfs_reader` 0.1.1 answers `unimplemented!()` from inside its
/// directory iterator for a device node, a fifo or a socket, which an
/// ordinary firmware image is full of. The panic is contained and reported,
/// and the message is about the image.
#[test]
fn a_device_node_is_reported_rather_than_taking_the_thread_down() {
    let tree = TempTree::new("device");
    let (_path, region) = at(
        &tree,
        &squashfs(vec![
            Item::File("top.txt", b"kept".to_vec()),
            Item::Device("console"),
        ]),
        0,
    );
    let (index, outcome) = index_of(&region);
    let err = outcome.expect_err("the skipped entry is reported");
    assert!(
        err.to_string().contains("1 entry was not listed"),
        "the refusal says what happened: {err}"
    );
    assert_eq!(
        names(&index, ""),
        vec!["top.txt".to_string()],
        "and the ordinary rows survive the entry that did not"
    );
}

/// A volume whose bytes stop halfway is reported, not panicked over.
#[test]
fn a_truncated_volume_is_reported() {
    let tree = TempTree::new("truncated");
    let volume = squashfs(self::tree());
    // Everything but the tables, which live at the end of the image.
    let cut = volume.len() / 2;
    let (_path, region) = at(&tree, volume.get(..cut).unwrap_or_default(), 0);
    let (index, outcome) = index_of(&region);
    assert!(outcome.is_err(), "a truncated volume cannot be walked");
    assert!(
        matches!(index.status(), IndexStatus::Failed(_)),
        "{:?}",
        index.status()
    );
}

/// A superblock full of the wrong bytes is damage, reported by name.
#[test]
fn a_corrupt_superblock_is_reported_as_damage() {
    let tree = TempTree::new("corrupt");
    let mut volume = squashfs(self::tree());
    // The block size and its log2 must agree or the volume is not one; this
    // makes them disagree, which is the check that catches a torn header.
    if let Some(slot) = volume.get_mut(12..14) {
        slot.copy_from_slice(&7u16.to_le_bytes());
    }
    let (_path, region) = at(&tree, &volume, 0);
    let (_index, outcome) = index_of(&region);
    let err = outcome.expect_err("a corrupt superblock cannot be opened");
    assert!(err.to_string().contains("damaged"), "{err}");
}

/// Zeroes are not a SquashFS and are refused as damage rather than read as an
/// empty volume.
#[test]
fn zeroes_are_not_a_squashfs() {
    let tree = TempTree::new("zeroes");
    let (_path, region) = at(&tree, &vec![0u8; 64 * 1024], 0);
    let (_index, outcome) = index_of(&region);
    let err = outcome.expect_err("zeroes are not a filesystem");
    assert!(err.to_string().contains("damaged"), "{err}");
}
