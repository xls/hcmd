//! ext2, ext3 and ext4, on `ext4-view`.
//!
//! One reader for the three, because they are one format with features added
//! rather than three formats: what tells them apart is a set of flags in the
//! superblock, and the layout a reader walks is the same. The sniffer names
//! the version and this reads all of them.
//!
//! # What ext has, and what this crate can see of it
//!
//! Mode bits, an owner, a group and symbolic links are real fields of the
//! inode and are reported. **A modification time is not available**: 0.9.3's
//! `Metadata` exposes the file type, the size, the mode, the uid and the gid
//! and nothing else, so the panel's date column is empty for every member of
//! an ext volume. That is the honest answer rather than an invented one, and
//! it is the only column of the panel this reader cannot fill.
//!
//! A name that is not UTF-8 is listed lossily, the way every other backend
//! decodes a name it did not choose. Such a member lists but cannot be read:
//! the lookup that finds it again is by the bytes on disk, and the replacement
//! characters are not those bytes.
//!
//! # Nothing is held open
//!
//! `ext4_view::Ext4` is an `Rc` around a `RefCell`, so it is neither `Send`
//! nor `Sync` while `Vfs` is both. A volume is opened, used and dropped inside
//! one call, always - the same rule the FAT reader keeps for the same reason.
//! It is also why [`Ext::open_member`] is not implemented: there is no handle
//! here that can leave the call it was made in, and the viewer's forward-only
//! mode is the honest answer.
//!
//! # Read-only, twice over
//!
//! `ext4-view` has no write path at all, and the handle underneath it is
//! [`super::block::Reader`], which refuses every write without touching the
//! file and is opened `O_RDONLY` besides.

use std::io::{Read as _, Seek as _, SeekFrom, Write};

use ext4_view::{Ext4, Ext4Error, FileType};

use crate::error::{Error, Result};
use crate::vfs::archive::index::{IndexSink, Locator, Member, MemberKind, RawMember};
use crate::vfs::{Capabilities, LatencyClass, PlainName, untrusted_mode};

use super::block::{Reader, Region};
use super::format::{self, FsId, MAX_DIR_DEPTH, VolumeFormat};

/// The buffer one member's bytes are copied through.
///
/// Nothing here is proportional to the member, the volume or the image: a
/// member larger than memory copies through this and no more. `Ext4::read`
/// exists and is not used for exactly that reason - it answers a `Vec` of the
/// whole file.
const COPY_BUFFER: usize = 64 * 1024;

/// The ext2/ext3/ext4 reader.
///
/// Zero-sized and `'static`, like every other [`VolumeFormat`]: everything
/// stateful belongs to the backend and the session.
#[derive(Debug)]
pub struct Ext;

impl VolumeFormat for Ext {
    /// Which filesystem this reads.
    ///
    /// The family, named by its newest member. Which of the three a volume is
    /// comes from the superblock's feature flags and is decided by the sniffer
    /// before this reader is chosen, so a message says ext2 for an ext2 image.
    fn id(&self) -> FsId {
        FsId::Ext4
    }

    /// What an ext volume offers.
    ///
    /// Not seekable and not randomly accessible: an `ext4_view::File` borrows
    /// the `Ext4` it came from, and that value is `Rc`-based and cannot cross
    /// a thread, so there is no handle to hand out.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            writable: false,
            seekable: false,
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

    /// Walk the whole volume, pushing every member as it is read.
    fn index(&self, region: &Region, sink: &mut dyn IndexSink) -> Result<()> {
        format::contained(&label(region), || walk(region, sink))
    }

    /// Copy one member's bytes into `out`.
    fn read_member(&self, region: &Region, member: &Member, out: &mut dyn Write) -> Result<u64> {
        format::contained(&label(region), || copy_member(region, member, out))
    }

    /// The volume label, for a status line. `None` when the volume has none.
    ///
    /// From the superblock, which is where `e2label` reads it. A label that is
    /// not UTF-8 is reported as no label rather than as replacement
    /// characters: a status line has one line and a name nobody typed is worse
    /// than none.
    fn volume_label(&self, region: &Region) -> Option<String> {
        let volume = load(region).ok()?;
        let text = volume.label().to_str().ok()?.trim();
        if text.is_empty() {
            return None;
        }
        Some(text.to_string())
    }
}

/// The window a volume's bytes are read through.
///
/// `ext4-view` asks for bytes at absolute offsets and this answers from the
/// region, so a volume on partition three reads partition three's bytes and a
/// read that would pass the partition's last byte fails rather than returning
/// its neighbour's.
struct Window(Reader);

impl ext4_view::Ext4Read for Window {
    /// Exactly `dst.len()` bytes, or an error.
    ///
    /// The contract the trait states, and a truncated image is exactly where
    /// it matters: a read past the end of the region is `UnexpectedEof` and
    /// becomes a reported refusal rather than a short buffer full of zeroes
    /// that the parser would go on to believe.
    fn read(
        &mut self,
        start_byte: u64,
        dst: &mut [u8],
    ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
        self.0.seek(SeekFrom::Start(start_byte)).map_err(Box::new)?;
        self.0.read_exact(dst).map_err(Box::new)?;
        Ok(())
    }
}

/// Open the volume on `region`, read-only.
///
/// Opened, used and dropped inside one call, never held: `Ext4` is `Rc`-based
/// and is neither `Send` nor `Sync`.
fn load(region: &Region) -> Result<Ext4> {
    let window = Window(region.open()?);
    Ext4::load(Box::new(window)).map_err(|err| match err {
        Ext4Error::Io(inner) => Error::msg(format!(
            "{}: the ext superblock could not be read ({inner})",
            label(region)
        )),
        // Everything else at this point is the superblock or the block group
        // descriptors: corrupt, or a feature this reader does not have.
        other => Error::msg(format!(
            "{}: the ext superblock is damaged ({other})",
            label(region)
        )),
    })
}

/// What a message calls this volume.
fn label(region: &Region) -> String {
    region.container().display().to_string()
}

/// The member path as the volume addresses it: absolute, `/`-separated.
fn member_path(member: &str) -> String {
    format!("/{member}")
}

/// The whole listing, depth first, pushed as it is read.
///
/// A stack of directories rather than recursion, each carrying the depth it
/// was found at: [`MAX_DIR_DEPTH`] is the bound, and a crafted image whose
/// directory entry points at an ancestor's inode reaches it instead of running
/// for ever. The index's own bounds cap the rest.
fn walk(region: &Region, sink: &mut dyn IndexSink) -> Result<()> {
    let volume = load(region)?;
    let mut pending = vec![(String::new(), 0usize)];
    while let Some((prefix, depth)) = pending.pop() {
        if depth >= MAX_DIR_DEPTH {
            return Err(too_deep(&prefix));
        }
        let listing = volume
            .read_dir(member_path(&prefix).as_str())
            .map_err(|err| trouble(dir_name(&prefix), err))?;
        for entry in listing {
            if sink.cancelled() {
                return Ok(());
            }
            let entry = entry.map_err(|err| trouble(dir_name(&prefix), err))?;
            // The name is chosen by whoever wrote the image and is folded into
            // a member path one line down, so nothing but one ordinary
            // component may reach that: `.` and `..`, which every ext
            // directory carries, are refused here along with anything holding
            // a separator.
            let name = String::from_utf8_lossy(entry.file_name().as_ref()).into_owned();
            let Some(name) = PlainName::new(name) else {
                continue;
            };
            let path = match prefix.is_empty() {
                true => name.into_string(),
                false => format!("{prefix}/{name}"),
            };
            let metadata = entry.metadata().map_err(|err| trouble(&path, err))?;
            let kind = match entry.file_type().map_err(|err| trouble(&path, err))? {
                FileType::Directory => MemberKind::Dir,
                FileType::Regular => MemberKind::File,
                FileType::Symlink => MemberKind::Symlink(link_target(&volume, &path)),
                FileType::BlockDevice
                | FileType::CharacterDevice
                | FileType::Fifo
                | FileType::Socket => MemberKind::Other,
            };
            let is_dir = matches!(kind, MemberKind::Dir);
            let raw = RawMember {
                name: path.clone(),
                kind,
                // A directory's size is the size of its own blocks, which is
                // not what the panel's size column means.
                size: if is_dir { 0 } else { metadata.len() },
                // 0.9.3 exposes no timestamp at all; see this module's header.
                mtime: None,
                mode: untrusted_mode(u32::from(metadata.mode())),
                uid: metadata.uid(),
                gid: metadata.gid(),
                // An ext member is not a byte range of the container: its
                // bytes are an extent tree, so there is no offset to record
                // and it is found again by walking to its path.
                locator: Locator::None,
            };
            if !sink.push(raw) {
                return Ok(());
            }
            if is_dir {
                pending.push((path, depth.saturating_add(1)));
            }
        }
    }
    Ok(())
}

/// Where a symbolic link points, as the image stores it.
///
/// An empty string for a target that could not be read or is not UTF-8, which
/// `ImageFs::read_link` already refuses to hand on. The link is never followed
/// here: `read_link` judges the target and the layer above decides what to do
/// with it.
fn link_target(volume: &Ext4, path: &str) -> String {
    let Ok(target) = volume.read_link(member_path(path).as_str()) else {
        return String::new();
    };
    target.to_str().map(str::to_string).unwrap_or_default()
}

/// Copy one member's bytes into `out`, counting them.
///
/// A symbolic link is refused rather than followed, so that reading a member
/// reads that member: `Ext4::open` would resolve the link and hand back a file
/// nobody asked for.
fn copy_member(region: &Region, member: &Member, out: &mut dyn Write) -> Result<u64> {
    let volume = load(region)?;
    let path = member_path(&member.path);
    let metadata = volume
        .symlink_metadata(path.as_str())
        .map_err(|err| trouble(&member.path, err))?;
    if metadata.is_dir() {
        return Err(Error::InvalidPath(format!(
            "{}: a directory has no contents to read",
            member.path
        )));
    }
    if metadata.is_symlink() {
        return Err(Error::InvalidPath(format!(
            "{}: a symbolic link has no contents of its own",
            member.path
        )));
    }
    let mut file = volume
        .open(path.as_str())
        .map_err(|err| trouble(&member.path, err))?;
    let mut buf = vec![0u8; COPY_BUFFER];
    let mut written = 0u64;
    loop {
        let read = file
            .read_bytes(&mut buf)
            .map_err(|err| trouble(&member.path, err))?;
        if read == 0 {
            break;
        }
        let Some(chunk) = buf.get(..read) else {
            break;
        };
        out.write_all(chunk)
            .map_err(|err| Error::msg(format!("{}: {err}", member.path)))?;
        written = written.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
    }
    Ok(written)
}

/// One `Ext4Error`, as this crate reports it.
///
/// The three states the design keeps apart are kept apart here: a path that is
/// not there is [`Error::NotFound`], a volume this reader cannot read is said
/// to be unsupported, and everything else is damage. A wildcard arm because
/// `Ext4Error` is `#[non_exhaustive]` and a new variant in a patch release
/// must not stop this crate compiling.
fn trouble(what: &str, err: Ext4Error) -> Error {
    match err {
        Ext4Error::NotFound => Error::NotFound(what.to_string()),
        Ext4Error::NotADirectory | Ext4Error::IsADirectory | Ext4Error::NotASymlink => {
            Error::InvalidPath(format!("{what}: {err}"))
        }
        Ext4Error::Incompatible(_) | Ext4Error::Encrypted => {
            Error::msg(format!("{what}: {err}, so it cannot be read here"))
        }
        Ext4Error::Corrupt(_) => Error::msg(format!("{what}: this ext volume is damaged ({err})")),
        other => Error::msg(format!("{what}: {other}")),
    }
}

/// What a message calls a directory, the root included.
fn dir_name(prefix: &str) -> &str {
    if prefix.is_empty() { "/" } else { prefix }
}

/// The refusal for a directory tree deeper than this reader walks.
fn too_deep(prefix: &str) -> Error {
    Error::msg(format!(
        "{}: this image nests directories deeper than {MAX_DIR_DEPTH}, \
         which a directory that contains itself also does",
        dir_name(prefix)
    ))
}

#[cfg(test)]
#[path = "ext_tests.rs"]
mod tests;
