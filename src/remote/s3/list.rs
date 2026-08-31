//! Reading a `ListObjectsV2` reply, and a `ListAllMyBuckets` one.
//!
//! Both are small, flat XML, read through [`crate::remote::xml`] by local name
//! so that an endpoint's choice of namespace does not matter.
//!
//! The reply is the server's to compose, so everything here is bounded and
//! nothing is trusted: a key that is not below the prefix it was asked for is
//! dropped rather than drawn at a path it does not occupy.

use crate::remote::xml::{first_text, split_elements};
use crate::vfs::{Entry, EntryKind};

/// One page of a listing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Page {
    /// The rows on this page.
    pub entries: Vec<Entry>,
    /// The token for the next page, when the reply says it was truncated.
    pub next: Option<String>,
}

/// Parse one `ListObjectsV2` reply.
///
/// `prefix` is what was asked for, and every key is made relative to it. A key
/// that does not begin with it is dropped: the panel is showing that prefix,
/// and a row from somewhere else would be drawn at a path it does not have.
#[must_use]
pub fn objects(xml: &str, prefix: &str, limit: usize) -> Page {
    let mut entries = Vec::new();

    // `<CommonPrefixes><Prefix>a/b/</Prefix></CommonPrefixes>` is a directory.
    for block in split_elements(xml, "CommonPrefixes") {
        if entries.len() >= limit {
            break;
        }
        let Some(full) = first_text(&block, "Prefix") else {
            continue;
        };
        let Some(name) = relative(&full, prefix) else {
            continue;
        };
        let name = name.trim_end_matches('/');
        if name.is_empty() || name.contains('/') {
            continue;
        }
        let mut entry = Entry::dir(name);
        entry.kind = EntryKind::Dir;
        entries.push(entry);
    }

    // `<Contents>` is an object.
    for block in split_elements(xml, "Contents") {
        if entries.len() >= limit {
            break;
        }
        let Some(key) = first_text(&block, "Key") else {
            continue;
        };
        let Some(name) = relative(&key, prefix) else {
            continue;
        };
        // The prefix marker itself: a zero-byte object at `a/b/` is how an
        // empty directory is made, and it is that directory rather than a file
        // inside it.
        if name.is_empty() || name.contains('/') {
            continue;
        }
        let mut entry = Entry::file(&name);
        entry.kind = EntryKind::File;
        entry.size = first_text(&block, "Size")
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        entry.mtime = first_text(&block, "LastModified")
            .as_deref()
            .and_then(iso8601);
        entries.push(entry);
    }

    Page {
        entries,
        next: first_text(xml, "IsTruncated")
            .filter(|t| t.trim().eq_ignore_ascii_case("true"))
            .and_then(|_| first_text(xml, "NextContinuationToken"))
            .map(|t| t.trim().to_string()),
    }
}

/// Every bucket in a `ListAllMyBuckets` reply, as directories.
///
/// The root of an S3 connection is the one listing that names buckets rather
/// than objects, which is what makes `/` browsable at all.
#[must_use]
pub fn buckets(xml: &str, limit: usize) -> Vec<Entry> {
    let mut out = Vec::new();
    for block in split_elements(xml, "Bucket") {
        if out.len() >= limit {
            break;
        }
        let Some(name) = first_text(&block, "Name") else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || name.contains('/') {
            continue;
        }
        let mut entry = Entry::dir(name);
        entry.kind = EntryKind::Dir;
        entry.mtime = first_text(&block, "CreationDate")
            .as_deref()
            .and_then(iso8601);
        out.push(entry);
    }
    out
}

/// The part of `key` below `prefix`, or `None` when it is not below it.
fn relative(key: &str, prefix: &str) -> Option<String> {
    key.strip_prefix(prefix).map(str::to_string)
}

/// `2024-05-24T00:00:00.000Z`, which is what S3 writes.
fn iso8601(text: &str) -> Option<std::time::SystemTime> {
    let stamp = chrono::DateTime::parse_from_rfc3339(text.trim()).ok()?;
    let secs = u64::try_from(stamp.timestamp()).ok()?;
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
}
