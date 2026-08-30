//! A file that is one compressed stream and nothing else.
//!
//! `.xz`, `.gz`, `.bz2` and `.zst` that do **not** hold a tar: a compressed
//! disk image, a compressed log, a compressed database dump. These used to be
//! refused with "a xz stream that does not contain a tar; hcmd browses
//! compressed tars, not singly compressed files", which is a true sentence
//! about the implementation and an unhelpful one about a
//! `raspios-trixie-arm64-lite.img.xz` sitting under the cursor.
//!
//! # It is a container with exactly one member
//!
//! Which is the whole design. Presented as an archive holding one file, the
//! rest of the program needs no new concept: `Enter` steps into it, `F3` views
//! the member, `F5` and `Alt+F6` copy it out through the ordinary copy engine
//! with its progress and conflict handling, and a `.img.xz` reached this way
//! goes on to open as a disk image, because a decompressed member is a file
//! like any other and the segment stack already nests.
//!
//! The member's name is the container's, with the compression suffix removed:
//! `disk.img.xz` holds `disk.img`. A container whose name carries no suffix to
//! remove holds a member named after the whole file, so there is always
//! exactly one row and it always has a name.
//!
//! # The size on that row
//!
//! Read from the container where the container states it, and **never** by
//! decompressing to find out - that is up to a few gigabytes of work to fill
//! in one column.
//!
//! * **xz** states it exactly, in the stream footer's index, which is a seek
//!   to the end and a few hundred bytes. [`liblzma::uncompressed_size`].
//! * **gzip** states it in the last four bytes, modulo 4 GiB, which is the
//!   same number `gzip -l` prints and has the same limitation.
//! * **zstd** states it in the frame header when the writer chose to, and
//!   nothing when it did not.
//! * **bzip2** does not state it anywhere.
//!
//! Where it is not stated the row reads `0`, which is this program's one
//! honest option: [`crate::vfs::Entry`] has no "unknown", and any other number
//! would be a claim about a file nothing has measured. The member still opens,
//! copies and views correctly, because every one of those paths streams to the
//! end rather than trusting the figure.

use std::io::{Read, Seek, Write};
use std::path::Path;

use super::format::{ArchiveFormat, Compression, FormatId, WriteModel};
use super::index::{IndexSink, Locator, RawMember};
use crate::error::{Error, Result};

/// One compressed stream, presented as an archive of one member.
#[derive(Debug, Clone, Copy)]
pub struct SingleFormat {
    /// Which decoder the stream needs.
    compression: Compression,
    /// Which row of the format table this is.
    id: FormatId,
}

/// `.gz` holding something other than a tar.
pub const GZ: SingleFormat = SingleFormat {
    compression: Compression::Gzip,
    id: FormatId::Gz,
};
/// `.bz2` holding something other than a tar.
pub const BZ2: SingleFormat = SingleFormat {
    compression: Compression::Bzip2,
    id: FormatId::Bz2,
};
/// `.xz` holding something other than a tar.
pub const XZ: SingleFormat = SingleFormat {
    compression: Compression::Xz,
    id: FormatId::Xz,
};
/// `.zst` holding something other than a tar.
pub const ZST: SingleFormat = SingleFormat {
    compression: Compression::Zstd,
    id: FormatId::Zst,
};

/// The member's name: the container's, without the compression suffix.
///
/// `disk.img.xz` holds `disk.img`. A name with nothing to strip holds a member
/// named after the whole container, because a container with no member at all
/// would be a listing with no rows and no way to reach the bytes.
#[must_use]
pub fn member_name(container: &str) -> String {
    let lower = container.to_ascii_lowercase();
    for suffix in [
        ".gz", ".bz2", ".xz", ".zst", ".zstd", ".tgz", ".tbz2", ".txz",
    ] {
        if lower.ends_with(suffix) && lower.len() > suffix.len() {
            return container
                .get(..container.len().saturating_sub(suffix.len()))
                .unwrap_or(container)
                .to_string();
        }
    }
    container.to_string()
}

impl SingleFormat {
    /// The uncompressed size the container states, or `0` where it states
    /// none.
    ///
    /// Never decompresses. See the module header for what each format records
    /// and what it costs to read it.
    fn stated_size(self, container: &Path) -> u64 {
        let Ok(mut file) = std::fs::File::open(container) else {
            return 0;
        };
        match self.compression {
            Compression::Xz => liblzma::uncompressed_size(&mut file).unwrap_or(0),
            Compression::Gzip => gzip_isize(&mut file),
            // bzip2 records no size, and a zstd frame header records one only
            // when the writer asked for it - which `zstd` does not do when it
            // is streaming, which is how most `.zst` files are made.
            Compression::Bzip2 | Compression::Zstd | Compression::None => 0,
        }
    }
}

/// gzip's `ISIZE`: the last four bytes, little endian, modulo 4 GiB.
///
/// The same figure `gzip -l` reports, with the same limitation - a member
/// larger than 4 GiB is reported as its size modulo 4 GiB, because that is all
/// the format stores. A multi-member gzip states only its last member's size,
/// so this is a lower bound there too.
fn gzip_isize(file: &mut std::fs::File) -> u64 {
    if file.seek(std::io::SeekFrom::End(-4)).is_err() {
        return 0;
    }
    let mut tail = [0_u8; 4];
    if file.read_exact(&mut tail).is_err() {
        return 0;
    }
    u64::from(u32::from_le_bytes(tail))
}

impl ArchiveFormat for SingleFormat {
    fn id(&self) -> FormatId {
        self.id
    }

    /// Read only. Rewriting the one member means recompressing the whole
    /// container, which is a copy and a rename rather than an edit, and this
    /// backend has no business pretending otherwise.
    fn write_model(&self) -> WriteModel {
        WriteModel::None
    }

    fn index(&self, container: &Path, sink: &mut dyn IndexSink) -> Result<()> {
        let name = container
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        sink.push(RawMember::file(
            member_name(&name),
            self.stated_size(container),
            // There is one member and it starts at the beginning of the
            // decompressed stream; its length is whatever the stream turns out
            // to be, which `read_member` discovers by reading to the end.
            Locator::Offset { data: 0, len: 0 },
        ));
        Ok(())
    }

    fn read_member(
        &self,
        container: &Path,
        _member: &super::index::Member,
        out: &mut dyn Write,
    ) -> Result<u64> {
        let file = std::fs::File::open(container).map_err(|e| Error::io(container, e))?;
        let mut stream = self.compression.decoder(file)?;
        std::io::copy(&mut stream, out).map_err(Error::Bare)
    }
}
