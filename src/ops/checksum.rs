//! Checksums: writing a sidecar, and checking one.
//!
//! Two jobs over the copy engine's own reader, because a checksum is a
//! streaming read and nothing else. Progress, cancellation and the failure
//! summary come from [`crate::ops::JobContext`] exactly as they do for a copy,
//! and no file is ever read whole into memory.
//!
//! # The two formats
//!
//! **`.sha256`** is the format `sha256sum` writes and reads: one line per
//! file, `<64 hex digits>  <name>`, two spaces between. Written by this
//! program, verified by `sha256sum -c`; written by `sha256sum`, verified here.
//! That interoperability is the whole point of choosing an existing format
//! rather than a better one.
//!
//! **`.sfv`** is the CRC32 format, which is older and simpler: `<name> <8 hex
//! digits>`, one space, and lines beginning `;` are comments. CRC32 is not a
//! cryptographic hash and is not offered as one; it is here because `.sfv`
//! files exist in the wild and something has to read them.
//!
//! # Paths in a sidecar are relative and shallow
//!
//! A sidecar names its files relative to itself, which is what makes a
//! directory and its `.sha256` movable together. A line naming `../secrets` or
//! `/etc/passwd` is refused rather than followed: a checksum file is data from
//! wherever it came from, and following a path out of the directory would let
//! one name a file to read.

use std::io::Read;

use crate::error::{Error, Result};
use crate::vfs::{Vfs, VfsPath};

/// Which digest a sidecar carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Digest {
    /// SHA-256, in a `.sha256` file.
    Sha256,
    /// CRC32, in a `.sfv` file.
    Crc32,
}

impl Digest {
    /// The extension a new sidecar gets, without the dot.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Crc32 => "sfv",
        }
    }

    /// What the status line and the summary call it.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Sha256 => "SHA-256",
            Self::Crc32 => "CRC32",
        }
    }

    /// The digest a sidecar's name implies, if any.
    #[must_use]
    pub fn of_name(name: &str) -> Option<Self> {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".sha256") {
            return Some(Self::Sha256);
        }
        if lower.ends_with(".sfv") {
            return Some(Self::Crc32);
        }
        None
    }
}

/// How many bytes are read at a time. The same window the copy engine uses.
const WINDOW: usize = 64 * 1024;

/// Hash one file, reporting progress and honouring cancellation.
///
/// `tick` is called per window with the bytes read so far; returning false
/// abandons the hash with [`Error::Cancelled`]. Never reads the file whole:
/// a checksum of a 40 GB file costs one window of memory.
pub fn hash_file(
    vfs: &dyn Vfs,
    path: &VfsPath,
    digest: Digest,
    tick: &mut dyn FnMut(u64) -> bool,
) -> Result<String> {
    let mut reader = vfs.open_read(path)?;
    let mut buf = vec![0_u8; WINDOW];
    let mut read = 0_u64;
    let mut sha = <sha2::Sha256 as sha2::Digest>::new();
    let mut crc = crc32fast::Hasher::new();

    loop {
        let got = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(Error::Bare(err)),
        };
        let Some(chunk) = buf.get(..got) else {
            break;
        };
        match digest {
            Digest::Sha256 => sha2::Digest::update(&mut sha, chunk),
            Digest::Crc32 => crc.update(chunk),
        }
        read = read.saturating_add(u64::try_from(got).unwrap_or(0));
        if !tick(read) {
            return Err(Error::Cancelled);
        }
    }

    Ok(match digest {
        Digest::Sha256 => sha2::Digest::finalize(sha)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        Digest::Crc32 => format!("{:08x}", crc.finalize()),
    })
}

/// One line of a sidecar: a name, and what it should hash to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The file's name, relative to the sidecar.
    pub name: String,
    /// The digest, lowercase hex.
    pub digest: String,
}

/// Write the lines of a sidecar in `digest`'s format.
#[must_use]
pub fn write_sidecar(entries: &[Entry], digest: Digest) -> String {
    let mut out = String::new();
    for entry in entries {
        match digest {
            // Two spaces, which is what `sha256sum` writes and what its `-c`
            // expects. One space means "text mode" to some implementations and
            // is the difference between a file that verifies and one that does
            // not.
            Digest::Sha256 => {
                out.push_str(&format!("{}  {}\n", entry.digest, entry.name));
            }
            Digest::Crc32 => {
                out.push_str(&format!("{} {}\n", entry.name, entry.digest));
            }
        }
    }
    out
}

/// Read the lines of a sidecar.
///
/// A line that is blank, a comment, or that cannot be read as an entry is
/// skipped: a sidecar with one bad line still verifies the rest, and reporting
/// the bad line is the caller's business rather than a reason to refuse the
/// whole file.
#[must_use]
pub fn read_sidecar(text: &str, digest: Digest) -> Vec<Entry> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        let parsed = match digest {
            // `<hash>  <name>`, and the name may contain spaces, so it is
            // everything after the first run of whitespace rather than the
            // second field.
            Digest::Sha256 => line
                .split_once(char::is_whitespace)
                .map(|(hash, rest)| Entry {
                    name: rest.trim().trim_start_matches('*').to_string(),
                    digest: hash.trim().to_ascii_lowercase(),
                }),
            // `<name> <hash>`, hash last, and the name may contain spaces, so
            // it is everything before the *last* run of whitespace.
            Digest::Crc32 => line
                .rsplit_once(char::is_whitespace)
                .map(|(rest, hash)| Entry {
                    name: rest.trim().to_string(),
                    digest: hash.trim().to_ascii_lowercase(),
                }),
        };
        let Some(entry) = parsed else {
            continue;
        };
        if entry.name.is_empty() || entry.digest.is_empty() {
            continue;
        }
        out.push(entry);
    }
    out
}

/// Whether a name in a sidecar may be read.
///
/// Relative, and below the sidecar's own directory. A sidecar is data from
/// wherever it was downloaded from, and a line naming `../../etc/passwd` would
/// otherwise be an instruction to read that file and report on it.
#[must_use]
pub fn name_is_safe(name: &str) -> bool {
    if name.is_empty() || name.starts_with('/') || name.starts_with('\\') {
        return false;
    }
    // Windows drive letters and UNC paths, which are absolute without a
    // leading separator.
    if name.len() > 1 && name.as_bytes().get(1) == Some(&b':') {
        return false;
    }
    !name
        .split(['/', '\\'])
        .any(|part| part == ".." || part == ".")
}

#[cfg(test)]
#[path = "checksum_tests.rs"]
mod tests;
