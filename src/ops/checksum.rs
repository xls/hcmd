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

/// The [`crate::ops::JobKind::Checksum`] worker, both ways round.
///
/// **Writing**: every source is hashed and one sidecar is written, at
/// `spec.dest`. The names in it are relative to that sidecar, so the directory
/// and its `.sha256` move together.
///
/// **Verifying**: `spec.sources` is the sidecar. Each line names a file beside
/// it, which is hashed and compared. A file that does not match is one entry
/// in [`crate::ops::JobSummary::differing`], which is the same place the
/// contents comparison puts its answer and is what the caller reports.
///
/// A file that cannot be read is a failure, not a mismatch: "could not be
/// read" and "is not what it should be" are different answers.
pub fn run(
    vfs: &dyn Vfs,
    spec: &crate::ops::JobSpec,
    ctx: &mut crate::ops::JobContext,
    verify: bool,
) {
    if verify {
        verify_sidecar(vfs, spec, ctx);
    } else {
        write_for(vfs, spec, ctx);
    }
}

/// Hash every source and write one sidecar.
fn write_for(vfs: &dyn Vfs, spec: &crate::ops::JobSpec, ctx: &mut crate::ops::JobContext) {
    let Some(dest) = spec.dest.as_ref() else {
        ctx.fail(
            &VfsPath::local(""),
            Error::msg("no file to write the checksums to"),
        );
        return;
    };
    let digest = Digest::of_name(&dest.display_title()).unwrap_or(Digest::Sha256);
    let sizes: Vec<u64> = spec
        .sources
        .iter()
        .map(|s| vfs.stat(s).map_or(0, |e| e.size))
        .collect();
    let total = sizes.iter().fold(0_u64, |a, n| a.saturating_add(*n));
    ctx.start(u64::try_from(spec.sources.len()).unwrap_or(u64::MAX), total);

    let mut entries: Vec<Entry> = Vec::new();
    for (index, source) in spec.sources.iter().enumerate() {
        if ctx.cancelled() {
            return;
        }
        let name = source.file_name().unwrap_or_else(|| source.to_string());
        ctx.set_file(&name, sizes.get(index).copied().unwrap_or(0));
        let mut last = 0_u64;
        let mut tick = |so_far: u64| -> bool {
            let delta = so_far.saturating_sub(last);
            last = so_far;
            ctx.add_bytes(delta)
        };
        match hash_file(vfs, source, digest, &mut tick) {
            Ok(hex) => {
                entries.push(Entry { name, digest: hex });
                ctx.add_file();
            }
            Err(Error::Cancelled) => return,
            Err(err) => ctx.fail(source, err),
        }
    }

    let body = write_sidecar(&entries, digest);
    match vfs.open_write(dest) {
        Ok(mut out) => {
            if let Err(err) = std::io::Write::write_all(&mut out, body.as_bytes()) {
                ctx.fail(dest, Error::Bare(err));
            }
        }
        Err(err) => ctx.fail(dest, err),
    }
}

/// Read a sidecar and check everything it names.
fn verify_sidecar(vfs: &dyn Vfs, spec: &crate::ops::JobSpec, ctx: &mut crate::ops::JobContext) {
    let Some(sidecar) = spec.sources.first() else {
        ctx.fail(
            &VfsPath::local(""),
            Error::msg("no checksum file to verify"),
        );
        return;
    };
    let name = sidecar.file_name().unwrap_or_else(|| sidecar.to_string());
    let Some(digest) = Digest::of_name(&name) else {
        ctx.fail(sidecar, Error::msg("not a .sha256 or .sfv file"));
        return;
    };
    let text = match read_to_string(vfs, sidecar) {
        Ok(text) => text,
        Err(err) => {
            ctx.fail(sidecar, err);
            return;
        }
    };
    let entries = read_sidecar(&text, digest);
    let base = sidecar.parent();
    ctx.start(u64::try_from(entries.len()).unwrap_or(u64::MAX), 0);

    for entry in entries {
        if ctx.cancelled() {
            return;
        }
        if !name_is_safe(&entry.name) {
            // A sidecar is data from wherever it came from. A line naming
            // `../../etc/passwd` is an instruction to read that file and
            // report on it, and it is refused by name rather than followed.
            ctx.add_differing(format!(
                "{} (refused: not a name beside the list)",
                entry.name
            ));
            ctx.add_file();
            continue;
        }
        let Some(base) = base.as_ref() else {
            ctx.fail(
                sidecar,
                Error::msg("nowhere to look for the files it names"),
            );
            return;
        };
        let path = base.join(&entry.name);
        ctx.set_file(&entry.name, vfs.stat(&path).map_or(0, |e| e.size));
        let mut tick = |_: u64| -> bool { !ctx.cancelled() };
        match hash_file(vfs, &path, digest, &mut tick) {
            Ok(hex) if hex.eq_ignore_ascii_case(&entry.digest) => ctx.add_file(),
            Ok(_) => {
                ctx.add_differing(entry.name.clone());
                ctx.add_file();
            }
            Err(Error::Cancelled) => return,
            Err(err) => ctx.fail(&path, err),
        }
    }
}

/// A whole small file, for the sidecar itself.
fn read_to_string(vfs: &dyn Vfs, path: &VfsPath) -> Result<String> {
    let mut reader = vfs.open_read(path)?;
    let mut bytes = Vec::new();
    // A sidecar is a list of names and hashes; one that is larger than this is
    // not one, and reading it whole would be the allocation this program does
    // not make on a number it read out of a file.
    reader
        .by_ref()
        .take(MAX_SIDECAR)
        .read_to_end(&mut bytes)
        .map_err(Error::Bare)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The most a checksum file may be: about 200,000 lines of SHA-256.
const MAX_SIDECAR: u64 = 32 * 1024 * 1024;

#[cfg(test)]
#[path = "checksum_tests.rs"]
mod tests;
