//! SquashFS, on `squashfs_reader`.
//!
//! The filesystem behind almost everything that ships as one file: an
//! AppImage, a Snap, an initramfs, and the firmware of most routers and
//! cameras. An image that holds one is therefore the common case rather than
//! an exotic one, which is why this reader exists before the exotic ones do.
//!
//! # What SquashFS has that FAT does not
//!
//! Mode bits, an owner, a group and a modification time, all of them real
//! fields of the inode rather than something inferred, and all of them
//! reported: the mode goes through [`crate::vfs::untrusted_mode`] and the
//! owner and group come from the volume's own id table. Nothing is invented
//! where the volume does not say - an id table lookup that fails leaves the
//! owner at `0` rather than failing the listing.
//!
//! # What cannot be listed, and why it is a skip rather than a failure
//!
//! **A symbolic link, a device node, a fifo and a socket cannot be listed at
//! all.** `squashfs_reader` 0.1.1 turns a directory entry's type tag into its
//! own `FileType` with an `unimplemented!()` for everything that is not a file
//! or a directory, and it does that inside the iterator, before this module
//! can see the entry and decide anything about it. There is no way round it
//! from outside that crate: the directory table is not reachable any other
//! way, and neither is its decompressor.
//!
//! What this module decides is the *shape of the loss*. Each entry is read
//! under [`format::contained`] on its own, so one symbolic link costs one row
//! rather than the whole image - an AppImage with a `.DirIcon` link still
//! browses - and the number skipped comes back as an error at the end of the
//! walk, which `ImageFs` reports underneath a listing that is otherwise
//! complete. Nothing is ever quietly missing.
//!
//! # Nothing is held open
//!
//! A volume is opened, used and dropped inside one call, exactly as the FAT
//! reader does it. `squashfs_reader` caches decompressed blocks inside the
//! `FileSystem` it opened them through, so a held volume would be a cache with
//! no lifetime and no owner; a fresh open costs the superblock and one
//! metadata block.
//!
//! The exception is [`Squashfs::open_member`], which hands out a reader that
//! owns its volume. That handle is the viewer's, it lives as long as the file
//! is on screen, and it is the reason a 2 GB file inside a SquashFS opens as
//! fast as a small one.
//!
//! # Why a panic is caught here
//!
//! `squashfs_reader` 0.1.1 answers `unimplemented!()` for the four inode
//! types it does not model - block device, character device, fifo and socket -
//! and it does so from inside the directory iterator, before this module can
//! see the entry and skip it. A firmware image with a `/dev/console` in it is
//! an ordinary image, not a crafted one, so the panic is contained
//! ([`format::contained`]) and reported as a refusal rather than left to take
//! a worker thread down. That is a defence against a library, not a licence
//! to panic here: nothing in this file panics.

use std::io::Write;

use squashfs_reader::{FileSystem, FileType};

use crate::error::{Error, Result};
use crate::vfs::archive::index::{IndexSink, Locator, Member, MemberKind, RawMember};
use crate::vfs::{Capabilities, LatencyClass, PlainName, ReadSeek, untrusted_mode};

use super::block::Region;
use super::format::{self, FsId, MAX_DIR_DEPTH, VolumeFormat};

/// The buffer one member's bytes are copied through.
///
/// Nothing here is proportional to the member, the volume or the image: a
/// member larger than memory copies through this and no more.
const COPY_BUFFER: usize = 64 * 1024;

/// The SquashFS reader.
///
/// Zero-sized and `'static`, like every other [`VolumeFormat`]: everything
/// stateful belongs to the backend and the session.
#[derive(Debug)]
pub struct Squashfs;

impl VolumeFormat for Squashfs {
    /// Which filesystem this reads.
    fn id(&self) -> FsId {
        FsId::Squashfs
    }

    /// What a SquashFS volume offers.
    ///
    /// Seekable and randomly accessible, which is the difference from FAT and
    /// is real rather than optimistic: a `FileReader` owns the volume it came
    /// from, so the handle can outlive the call that made it, and the format
    /// stores a file as a list of independently addressable blocks, so a seek
    /// costs one block rather than a walk from the first byte.
    ///
    /// Not writable, because a disk image has no write path at all, and not
    /// executable, because a file inside an image has no path the kernel can
    /// be handed.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            writable: false,
            seekable: true,
            random_access: true,
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
    ///
    /// A fresh open and a fresh lookup, because nothing is held between
    /// calls. The caller has already wrapped `out` in the archive backend's
    /// guard, so a member whose inode lies about its size stops at the claim.
    fn read_member(&self, region: &Region, member: &Member, out: &mut dyn Write) -> Result<u64> {
        format::contained(&label(region), || copy_member(region, member, out))
    }

    /// A seekable handle on one member.
    ///
    /// `Some` for a regular file and `None` for anything else. The handle owns
    /// the volume it reads through, so it is a `Send` reader in its own right
    /// and the viewer can hold it for as long as the file is open.
    fn open_member(
        &self,
        region: &Region,
        member: &Member,
    ) -> Result<Option<Box<dyn ReadSeek + Send>>> {
        if !matches!(member.kind, MemberKind::File) {
            return Ok(None);
        }
        format::contained(&label(region), || {
            let volume =
                FileSystem::from_read(region.open()?).map_err(|err| damaged(region, err))?;
            let metadata = match inode_of(member) {
                Some(inode) => volume.metadata_from_ref(inode),
                None => volume.metadata(member_path(member)),
            }
            .map_err(|err| missing(&member.path, err))?;
            if !metadata.is_file() {
                return Ok(None);
            }
            let reader = volume
                .open_from_metadata(&metadata)
                .map_err(|err| Error::msg(format!("{}: {err}", member.path)))?;
            Ok(Some(Box::new(reader) as Box<dyn ReadSeek + Send>))
        })
    }
}

/// The refusal for a volume whose superblock did not parse.
///
/// A function rather than a shared `open`, and the reason is worth writing
/// down: `squashfs_reader::FileSystem::from_read` answers
/// `FileSystem<SharedReader<Reader>>`, and `SharedReader` is not exported by
/// that crate, so the type has no name a signature here could use. Every
/// caller therefore opens the volume in its own `let` - where inference does
/// not need the name - and shares this message.
fn damaged(region: &Region, err: std::io::Error) -> Error {
    Error::msg(format!(
        "{}: the SquashFS superblock is damaged ({err})",
        label(region)
    ))
}

/// The member path as the volume addresses it: absolute, `/`-separated.
///
/// Every component of [`Member::path`] is one name `index::Builder` already
/// accepted, so this cannot produce a `.` or a `..` for the lookup to follow,
/// and `squashfs_reader` refuses both in any case.
fn member_path(member: &Member) -> String {
    format!("/{}", member.path)
}

/// What a message calls this volume.
fn label(region: &Region) -> String {
    region.container().display().to_string()
}

/// The whole listing, depth first, pushed as it is read.
///
/// A stack of directories rather than recursion, and each entry carries the
/// depth it was found at: [`MAX_DIR_DEPTH`] is the bound, and a crafted image
/// whose directory is its own ancestor reaches it instead of running for ever.
/// A directory listing cannot itself be endless - `squashfs_reader` reads a
/// directory as a byte count that only decreases - so there is no second
/// bound to keep here, and the index's own [`IndexSink`] bounds the rest.
///
/// # One entry at a time, contained
///
/// Every step of the iterator is taken under [`format::contained`], one entry
/// at a time, and this is the whole reason the containment is here rather than
/// only around the walk: `squashfs_reader` 0.1.1 converts a directory entry's
/// type tag into its own `FileType` with an `unimplemented!()` for everything
/// that is not a file or a directory, so **a symbolic link, a device node, a
/// fifo or a socket panics as it is read**. Containing the whole walk would
/// mean that one symbolic link anywhere made a whole image unbrowsable, and
/// AppImages and Snaps have symbolic links in them.
///
/// So the entry is skipped, the rest of the directory and the rest of the
/// image still list, and the count comes back as an error at the end - which
/// `ImageFs` reports under a listing that is complete except for those rows,
/// rather than instead of it. A skipped entry is missing from the panel, but
/// it is never missing quietly.
///
/// Resuming after the panic is sound rather than hopeful: the iterator reads
/// the entry, advances its own byte count and its header counter, and only
/// then builds the value that panics, so the state it is left in is the state
/// it would have had if the entry had been returned.
fn walk(region: &Region, sink: &mut dyn IndexSink) -> Result<()> {
    let volume = FileSystem::from_read(region.open()?).map_err(|err| damaged(region, err))?;
    let root = volume
        .metadata("/")
        .map_err(|err| Error::msg(format!("{}: {err}", label(region))))?;
    let mut pending = vec![(String::new(), 0usize, root)];
    // How many entries the library could not represent, and the first
    // directory one was met in.
    let mut skipped = 0usize;
    let mut unrepresented = String::new();
    while let Some((prefix, depth, metadata)) = pending.pop() {
        if depth >= MAX_DIR_DEPTH {
            return Err(too_deep(&prefix));
        }
        let mut listing = volume
            .read_dir_from_metadata(&metadata)
            .map_err(|err| Error::msg(format!("{}: {err}", dir_name(&prefix))))?;
        loop {
            if sink.cancelled() {
                return Ok(());
            }
            let step = format::contained(dir_name(&prefix), || Ok(listing.next()));
            let entry = match step {
                Ok(Some(Ok(entry))) => entry,
                Ok(Some(Err(err))) => {
                    return Err(Error::msg(format!("{}: {err}", dir_name(&prefix))));
                }
                Ok(None) => break,
                Err(_) => {
                    if unrepresented.is_empty() {
                        unrepresented = dir_name(&prefix).to_string();
                    }
                    skipped = skipped.saturating_add(1);
                    continue;
                }
            };
            // The name is chosen by whoever wrote the image and is folded into
            // a member path one line down, so nothing but one ordinary
            // component may reach that. A row that is not one is dropped and
            // the rest of the volume is still listed.
            let Some(name) = PlainName::new(entry.name()) else {
                continue;
            };
            let path = match prefix.is_empty() {
                true => name.into_string(),
                false => format!("{prefix}/{name}"),
            };
            let metadata = entry
                .metadata(&volume)
                .map_err(|err| Error::msg(format!("{path}: {err}")))?;
            let kind = match metadata.file_type() {
                FileType::Directory => MemberKind::Dir,
                FileType::File => MemberKind::File,
                // Not reachable with 0.1.1, which panics on the directory
                // entry before its inode is ever read (see this module's
                // header). It is written out rather than left to a wildcard so
                // that a library which learns the type lists links correctly
                // instead of listing them as files.
                FileType::Symlink => {
                    MemberKind::Symlink(metadata.target().unwrap_or_default().to_string())
                }
            };
            let is_dir = matches!(kind, MemberKind::Dir);
            let raw = RawMember {
                name: path.clone(),
                kind,
                // A directory's own size is the size of its listing, which is
                // not what the panel's size column means.
                size: if is_dir { 0 } else { metadata.len() },
                mtime: Some(metadata.modified()),
                mode: untrusted_mode(u32::from(metadata.permissions())),
                // The id table is a lookup the image can get wrong. An owner
                // that cannot be read is left at zero rather than failing a
                // listing over a column.
                uid: metadata.uid(&volume).unwrap_or(0),
                gid: metadata.gid(&volume).unwrap_or(0),
                // Not a byte range of the container - a SquashFS file is a
                // list of compressed blocks - but not nothing either: this is
                // the inode's own reference, which is what reading the member
                // later resolves it by. It saves a directory scan per read,
                // and it avoids one: a scan walks past its neighbours, and a
                // neighbour this library cannot represent panics.
                locator: locator_for(entry.inode_ref()),
            };
            if !sink.push(raw) {
                return Ok(());
            }
            if is_dir {
                pending.push((path, depth.saturating_add(1), metadata));
            }
        }
    }
    if skipped > 0 {
        return Err(Error::msg(format!(
            "{unrepresented}: {skipped} entr{} not listed - this SquashFS \
             reader represents files and directories, and this image holds \
             something else (a symbolic link, a device node, a fifo or a \
             socket)",
            if skipped == 1 { "y was" } else { "ies were" }
        )));
    }
    Ok(())
}

/// How a member is found again: its inode reference, when that fits.
///
/// `Locator::Ordinal` is "the member's position in the container's own order",
/// and a SquashFS inode reference is exactly that - an opaque handle the
/// volume resolves. It is a `u64` and the locator holds a `usize`, so on a
/// 32-bit target a reference past 4 GiB does not fit and the member is found
/// by its path instead, which is slower and still correct.
fn locator_for(inode: squashfs_reader::MetadataRef) -> Locator {
    match usize::try_from(inode.into_inner()) {
        Ok(raw) => Locator::Ordinal(raw),
        Err(_) => Locator::None,
    }
}

/// The inode reference the index kept for `member`, if it kept one.
///
/// The lookup itself is written out at both call sites rather than shared,
/// for the reason [`damaged`] gives: the volume's type has no name a
/// signature here could use.
fn inode_of(member: &Member) -> Option<squashfs_reader::MetadataRef> {
    match member.locator {
        Locator::Ordinal(raw) => Some(squashfs_reader::MetadataRef::from(
            u64::try_from(raw).unwrap_or_default(),
        )),
        Locator::Offset { .. } | Locator::None => None,
    }
}

/// Copy one member's bytes into `out`, counting them.
///
/// A symbolic link is refused rather than followed. The target is a string the
/// image chose and following it here would read a file the caller did not ask
/// for; `Vfs::read_link` is where a link is answered, and it judges the target
/// before it hands it back.
fn copy_member(region: &Region, member: &Member, out: &mut dyn Write) -> Result<u64> {
    let volume = FileSystem::from_read(region.open()?).map_err(|err| damaged(region, err))?;
    let metadata = match inode_of(member) {
        Some(inode) => volume.metadata_from_ref(inode),
        None => volume.metadata(member_path(member)),
    }
    .map_err(|err| missing(&member.path, err))?;
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
    let mut reader = volume
        .open_from_metadata(&metadata)
        .map_err(|err| Error::msg(format!("{}: {err}", member.path)))?;
    let mut buf = vec![0u8; COPY_BUFFER];
    let mut written = 0u64;
    loop {
        let read = match std::io::Read::read(&mut reader, &mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(Error::msg(format!("{}: {err}", member.path))),
        };
        let Some(chunk) = buf.get(..read) else {
            break;
        };
        out.write_all(chunk)
            .map_err(|err| Error::msg(format!("{}: {err}", member.path)))?;
        written = written.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
    }
    Ok(written)
}

/// A lookup that failed: not found where the volume says so, damaged
/// otherwise.
fn missing(path: &str, err: std::io::Error) -> Error {
    match err.kind() {
        std::io::ErrorKind::NotFound => Error::NotFound(path.to_string()),
        _ => Error::msg(format!("{path}: {err}")),
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
#[path = "squashfs_tests.rs"]
mod tests;
