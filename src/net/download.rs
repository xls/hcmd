//! Streaming file downloads: the one long, user-initiated HTTP this program
//! does.
//!
//! Everything else in [`crate::net`] is a short question answered into memory.
//! A download is the opposite: an arbitrary number of bytes, over an arbitrary
//! time, that must be cancellable and resumable. So it streams the body to a
//! file a chunk at a time, reporting progress and asking permission to carry
//! on after each chunk, and it speaks `Range` so an interrupted file is
//! continued rather than fetched again.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use crate::error::{Error, Result};

/// How a finished [`download`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadStatus {
    /// The whole file is on disk.
    Completed,
    /// The caller's progress callback asked to stop. The partial file is left
    /// in place, so a later call with `resume` continues it.
    Cancelled,
}

/// One read/write chunk: small enough that a cancel is noticed promptly, large
/// enough not to syscall per byte.
const CHUNK: usize = 64 * 1024;

/// What the response says to do with the file before the body is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// The range asked for is already satisfied (`416`): nothing to fetch.
    Complete,
    /// Write the body from `offset` - `0` truncates and rewrites the whole
    /// file - with `total` bytes expected when the server stated a size.
    Write {
        /// Where in the file the body begins.
        offset: u64,
        /// The finished file's size, when the server said so.
        total: Option<u64>,
    },
}

/// Decide where the body goes, from the offset we asked to resume at and the
/// response we got.
///
/// `from` is where the caller asked the server to continue (`0` = a fresh GET,
/// no `Range`). A server that honours the range answers `206`; one that ignores
/// it answers `200` with the whole file, and the file is rewritten from the
/// start rather than corrupted by appending a second copy.
fn plan(
    from: u64,
    status: u16,
    content_length: Option<u64>,
    range_total: Option<u64>,
) -> Result<Plan> {
    match status {
        // Range honoured: continue from where we left off.
        206 => Ok(Plan::Write {
            offset: from,
            total: range_total.or_else(|| content_length.map(|len| from.saturating_add(len))),
        }),
        // The whole file: either we asked for all of it, or the server ignored
        // the range. Either way it is rewritten from the start.
        200 => Ok(Plan::Write {
            offset: 0,
            total: content_length,
        }),
        // The partial file is already as long as the resource has to give.
        416 => Ok(Plan::Complete),
        // Any other success is treated as a whole-file answer rather than
        // trusted to have honoured a range it did not signal.
        other if (200..300).contains(&other) => Ok(Plan::Write {
            offset: 0,
            total: content_length,
        }),
        other => Err(Error::msg(format!("the server answered {other}"))),
    }
}

/// The total size out of a `Content-Range: bytes 200-999/1000` header, or
/// `None` when it is absent or unknown (`*`).
fn range_total(value: &str) -> Option<u64> {
    let total = value.rsplit('/').next()?.trim();
    if total == "*" {
        return None;
    }
    total.parse().ok()
}

/// The bytes already on disk for `path`, or `0` when it is not there.
fn existing_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Open `path` to receive the body: truncated when writing from the start,
/// appended to when resuming.
fn open_at(path: &Path, offset: u64) -> Result<File> {
    if offset == 0 {
        File::create(path).map_err(|e| Error::io(path, e))
    } else {
        OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|e| Error::io(path, e))
    }
}

/// A header's value as a `u64`, when it is one.
fn header_u64(response: &ureq::http::Response<ureq::Body>, name: &str) -> Option<u64> {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse().ok())
}

/// The `GET`, following redirects, with the resume `Range` when there is one.
fn request(url: &str, from: u64) -> Result<ureq::http::Response<ureq::Body>> {
    // No global timeout: a download is long by nature, and the job that drives
    // it is cancellable, which is the stop this needs rather than a clock.
    let config = ureq::Agent::config_builder()
        .timeout_global(None)
        .user_agent(super::agent())
        .build();
    let http = ureq::Agent::new_with_config(config);
    let mut req = http.get(url);
    if from > 0 {
        req = req.header("Range", &format!("bytes={from}-"));
    }
    req.call().map_err(|e| Error::msg(format!("{url}: {e}")))
}

/// Stream `url` into `path`, following redirects.
///
/// With `resume`, an existing `path` is continued from its current length; a
/// server that ignores the range restarts the file cleanly. Without `resume`,
/// any existing file is truncated first. `progress(done, total)` is called as
/// bytes land - `total` is the finished size when the server states it - and
/// returning `false` stops and leaves the partial file for a later resume.
///
/// `path`'s parent directory must already exist; the caller owns where
/// downloads live.
pub fn download(
    url: &str,
    path: &Path,
    resume: bool,
    progress: &mut dyn FnMut(u64, Option<u64>) -> bool,
) -> Result<DownloadStatus> {
    let from = if resume { existing_len(path) } else { 0 };
    let mut response = request(url, from)?;
    let status = response.status().as_u16();
    let content_length = header_u64(&response, "content-length");
    let range = response
        .headers()
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        .and_then(range_total);

    let (offset, total) = match plan(from, status, content_length, range)
        .map_err(|e| Error::msg(format!("{url}: {e}")))?
    {
        Plan::Complete => return Ok(DownloadStatus::Completed),
        Plan::Write { offset, total } => (offset, total),
    };

    let mut file = open_at(path, offset)?;
    let mut done = offset;
    let mut reader = response.body_mut().as_reader();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| Error::msg(format!("{url}: {e}")))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| Error::io(path, e))?;
        done = done.saturating_add(n as u64);
        if !progress(done, total) {
            let _ = file.flush();
            return Ok(DownloadStatus::Cancelled);
        }
    }
    file.flush().map_err(|e| Error::io(path, e))?;
    let _ = file.sync_all();
    Ok(DownloadStatus::Completed)
}

#[cfg(test)]
#[path = "download_tests.rs"]
mod tests;
