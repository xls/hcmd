//! Splitting a file into numbered parts, and putting them back.
//!
//! The classic commander pair, and pure I/O: two more jobs over the same
//! engine as copy, so progress, cancellation and the failure summary come for
//! free and nothing is read whole.
//!
//! # The naming
//!
//! `archive.iso` becomes `archive.iso.001`, `archive.iso.002`, and so on. That
//! is what Total Commander writes, what `7z` writes, and what every tool that
//! reads split files expects; three digits because two runs out at 99 parts
//! and a file that needs more than 999 wants a different size rather than a
//! different scheme.
//!
//! Merging takes the **first** part and finds the rest by counting up from it.
//! It stops at the first number that is missing, and says which one that was:
//! a merge that silently produced a short file would be worse than no merge,
//! because the result looks like a file and is not one.

use std::io::{Read, Write};

use crate::error::Error;
use crate::vfs::{Vfs, VfsPath};

/// How many bytes are moved at a time.
const WINDOW: usize = 64 * 1024;

/// The most parts one split may produce.
///
/// A guard against a part size of zero or one byte, which would otherwise ask
/// the filesystem for as many files as the source has bytes.
pub const MAX_PARTS: usize = 999;

/// The name of part `n`, counting from one.
#[must_use]
pub fn part_name(stem: &str, n: usize) -> String {
    format!("{stem}.{n:03}")
}

/// Whether a name looks like the first part of a split set.
///
/// `.001` and nothing else: `.002` is a part but not a starting point, and
/// offering to merge from the middle would produce a file missing its head.
#[must_use]
pub fn is_first_part(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(stem, ext)| !stem.is_empty() && ext == "001")
}

/// The stem a set of parts belongs to: `a.iso.001` is `a.iso`.
#[must_use]
pub fn stem_of_part(name: &str) -> Option<String> {
    name.rsplit_once('.')
        .filter(|(stem, ext)| {
            !stem.is_empty() && ext.len() == 3 && ext.bytes().all(|b| b.is_ascii_digit())
        })
        .map(|(stem, _)| stem.to_string())
}

/// The [`crate::ops::JobKind::Split`] worker.
///
/// `spec.sources[0]` is the file, `spec.dest` is the directory to write into,
/// and `spec.options.part_size` is how much goes in each part.
pub fn run_split(vfs: &dyn Vfs, spec: &crate::ops::JobSpec, ctx: &mut crate::ops::JobContext) {
    let Some(source) = spec.sources.first() else {
        ctx.fail(&VfsPath::local(""), Error::msg("nothing to split"));
        return;
    };
    let Some(dest) = spec.dest.as_ref() else {
        ctx.fail(source, Error::msg("nowhere to write the parts"));
        return;
    };
    let size = spec.options.part_size;
    if size == 0 {
        ctx.fail(source, Error::msg("a part size of zero would never finish"));
        return;
    }

    let total = vfs.stat(source).map_or(0, |e| e.size);
    let parts = total.div_ceil(size).max(1);
    if parts > MAX_PARTS as u64 {
        ctx.fail(
            source,
            Error::msg(format!(
                "that size needs {parts} parts, and the numbering stops at {MAX_PARTS}; \
                 use a larger part size"
            )),
        );
        return;
    }
    ctx.start(parts, total);

    let stem = source.file_name().unwrap_or_else(|| "split".to_string());
    let mut reader = match vfs.open_read(source) {
        Ok(reader) => reader,
        Err(err) => {
            ctx.fail(source, err);
            return;
        }
    };

    let mut buf = vec![0_u8; WINDOW];
    let mut index = 1_usize;
    let mut done = false;
    while !done {
        if ctx.cancelled() {
            return;
        }
        let path = dest.join(part_name(&stem, index));
        ctx.set_file(&part_name(&stem, index), size.min(total));
        let mut out = match vfs.open_write(&path) {
            Ok(out) => out,
            Err(err) => {
                ctx.fail(&path, err);
                return;
            }
        };
        let mut written = 0_u64;
        while written < size {
            let want = usize::try_from(size.saturating_sub(written))
                .unwrap_or(WINDOW)
                .min(WINDOW);
            let Some(slice) = buf.get_mut(..want) else {
                break;
            };
            match reader.read(slice) {
                Ok(0) => {
                    done = true;
                    break;
                }
                Ok(n) => {
                    let Some(chunk) = buf.get(..n) else {
                        break;
                    };
                    if let Err(err) = out.write_all(chunk) {
                        ctx.fail(&path, Error::Bare(err));
                        return;
                    }
                    written = written.saturating_add(u64::try_from(n).unwrap_or(0));
                    if !ctx.add_bytes(u64::try_from(n).unwrap_or(0)) {
                        return;
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) => {
                    ctx.fail(&path, Error::Bare(err));
                    return;
                }
            }
        }
        if let Err(err) = out.flush() {
            ctx.fail(&path, Error::Bare(err));
            return;
        }
        drop(out);
        // A final part that got nothing is not a part: it is the loop noticing
        // the source ended on a boundary, and leaving a zero-byte `.004`
        // behind would make the set look longer than it is.
        if written == 0 && index > 1 {
            let _ = vfs.remove(&path);
            break;
        }
        ctx.add_file();
        index = index.saturating_add(1);
    }
}

/// The [`crate::ops::JobKind::Merge`] worker.
///
/// `spec.sources[0]` is the **first** part; the rest are found by counting up.
pub fn run_merge(vfs: &dyn Vfs, spec: &crate::ops::JobSpec, ctx: &mut crate::ops::JobContext) {
    let Some(first) = spec.sources.first() else {
        ctx.fail(&VfsPath::local(""), Error::msg("nothing to merge"));
        return;
    };
    let name = first.file_name().unwrap_or_default();
    let Some(stem) = stem_of_part(&name) else {
        ctx.fail(first, Error::msg("that is not a numbered part"));
        return;
    };
    let Some(dir) = first.parent() else {
        ctx.fail(first, Error::msg("that part has nowhere to merge into"));
        return;
    };
    let target = spec.dest.clone().unwrap_or_else(|| dir.join(stem.clone()));

    // Count the set first, so the progress bar is about the whole merge rather
    // than about each part in turn, and so a missing part is found before
    // anything has been written.
    let mut parts: Vec<VfsPath> = Vec::new();
    let mut total = 0_u64;
    for index in 1..=MAX_PARTS {
        let path = dir.join(part_name(&stem, index));
        match vfs.stat(&path) {
            Ok(entry) => {
                total = total.saturating_add(entry.size);
                parts.push(path);
            }
            Err(_) if index > 1 => break,
            Err(err) => {
                ctx.fail(&path, err);
                return;
            }
        }
    }
    ctx.start(u64::try_from(parts.len()).unwrap_or(0), total);

    let mut out = match vfs.open_write(&target) {
        Ok(out) => out,
        Err(err) => {
            ctx.fail(&target, err);
            return;
        }
    };
    let mut buf = vec![0_u8; WINDOW];
    for path in &parts {
        if ctx.cancelled() {
            return;
        }
        ctx.set_file(
            &path.file_name().unwrap_or_default(),
            vfs.stat(path).map_or(0, |e| e.size),
        );
        let mut reader = match vfs.open_read(path) {
            Ok(reader) => reader,
            Err(err) => {
                ctx.fail(path, err);
                return;
            }
        };
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let Some(chunk) = buf.get(..n) else {
                        break;
                    };
                    if let Err(err) = out.write_all(chunk) {
                        ctx.fail(&target, Error::Bare(err));
                        return;
                    }
                    if !ctx.add_bytes(u64::try_from(n).unwrap_or(0)) {
                        return;
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) => {
                    ctx.fail(path, Error::Bare(err));
                    return;
                }
            }
        }
        ctx.add_file();
    }
    if let Err(err) = out.flush() {
        ctx.fail(&target, Error::Bare(err));
    }
}

#[cfg(test)]
#[path = "split_tests.rs"]
mod tests;
