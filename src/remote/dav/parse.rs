//! Reading a `PROPFIND` multistatus.
//!
//! Written here rather than with an XML crate. A multistatus is a handful of
//! elements, and the awkward part is not the grammar but the namespaces:
//! servers spell the DAV namespace `D:`, `d:`, `lp1:` or nothing at all, and a
//! parser that cared would need configuring per server. Matching on the local
//! name and ignoring the prefix is both simpler and more forgiving, which is
//! the right trade for a format this shape.
//!
//! Everything here treats the reply as hostile. A server that names
//! `../../etc` or a path on another host is refused rather than followed.

use crate::vfs::{Entry, EntryKind};

/// One `<response>` from a multistatus, reduced to what a panel shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The path from `<href>`, percent-decoded, absolute on this server.
    pub href: String,
    /// True when `<resourcetype>` held `<collection/>`.
    pub is_dir: bool,
    /// `<getcontentlength>`, or zero.
    pub len: u64,
    /// `<getlastmodified>` as it was written, for the caller to parse.
    pub modified: Option<String>,
}

/// The local name of an element, prefix discarded.
fn local(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

/// Percent-decode, leaving anything malformed as it was written.
///
/// A server is allowed to send `%2F` inside a segment, and a name with a
/// literal `%` in it is not an error either.
#[must_use]
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes.get(i).copied().unwrap_or(b'%');
        if byte == b'%'
            && let (Some(hi), Some(lo)) = (bytes.get(i + 1), bytes.get(i + 2))
            && let (Some(hi), Some(lo)) =
                (char::from(*hi).to_digit(16), char::from(*lo).to_digit(16))
        {
            out.push(u8::try_from(hi * 16 + lo).unwrap_or(b'?'));
            i += 3;
            continue;
        }
        out.push(byte);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Every `<response>` in a multistatus, in the order the server wrote them.
///
/// Stops at `limit`, which is the caller's bound on how much of a reply it
/// will allocate from.
#[must_use]
pub fn multistatus(xml: &str, limit: usize) -> Vec<Response> {
    let mut out = Vec::new();
    for block in split_elements(xml, "response") {
        if out.len() >= limit {
            break;
        }
        let Some(href) = first_text(&block, "href") else {
            continue;
        };
        // A `<propstat>` with a non-2xx status describes properties the server
        // declined to give, not a file that does not exist. The href is still
        // the file, so the row is kept with whatever was readable.
        let is_dir = block_has_collection(&block);
        let len = first_text(&block, "getcontentlength")
            .and_then(|t| t.trim().parse::<u64>().ok())
            .unwrap_or(0);
        out.push(Response {
            href: percent_decode(href.trim()),
            is_dir,
            len,
            modified: first_text(&block, "getlastmodified").map(|s| s.trim().to_string()),
        });
    }
    out
}

/// Whether `<resourcetype>` in this block contains `<collection/>`.
fn block_has_collection(block: &str) -> bool {
    let Some(kind) = first_element(block, "resourcetype") else {
        return false;
    };
    split_tags(&kind).any(|tag| local(&tag) == "collection")
}

/// The text of the first `<name>` element, prefix-insensitive.
fn first_text(xml: &str, name: &str) -> Option<String> {
    first_element(xml, name).map(|inner| unescape(&inner))
}

/// The inner text of the first element with this local name.
fn first_element(xml: &str, name: &str) -> Option<String> {
    let mut rest = xml;
    loop {
        let open = rest.find('<')?;
        let after = rest.get(open + 1..)?;
        let close = after.find('>')?;
        let tag = after.get(..close)?;
        let bare = tag
            .trim_end_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or(tag);
        if !bare.starts_with('/') && local(bare) == name && !tag.ends_with('/') {
            let body = after.get(close + 1..)?;
            let end = find_close(body, name)?;
            return body.get(..end).map(str::to_string);
        }
        rest = after.get(close + 1..)?;
    }
}

/// Where the matching close tag for `name` starts.
fn find_close(body: &str, name: &str) -> Option<usize> {
    let mut at = 0;
    loop {
        let open = body.get(at..)?.find("</")? + at;
        let after = body.get(open + 2..)?;
        let close = after.find('>')?;
        let tag = after.get(..close)?;
        if local(tag.trim()) == name {
            return Some(open);
        }
        at = open + 2 + close;
    }
}

/// Every `<name> ... </name>` block in the document.
fn split_elements(xml: &str, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(found) = first_element(rest, name) {
        // Advance past this one. The close tag is the safest anchor: the block
        // itself may contain the same text as the document elsewhere.
        let Some(at) = rest.find(&found) else {
            break;
        };
        out.push(found.clone());
        let Some(next) = rest.get(at + found.len()..) else {
            break;
        };
        rest = next;
    }
    out
}

/// Every tag name inside a fragment.
fn split_tags(fragment: &str) -> impl Iterator<Item = String> + '_ {
    fragment.split('<').filter_map(|piece| {
        let tag = piece.split('>').next()?;
        let bare = tag.trim().trim_end_matches('/').split_whitespace().next()?;
        (!bare.is_empty() && !bare.starts_with('/') && !bare.starts_with('?'))
            .then(|| bare.to_string())
    })
}

/// The five XML entities, which is all a `getlastmodified` or an `href` can
/// legally contain.
fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Turn one response into a panel row, relative to the directory listed.
///
/// `None` for the directory itself, which every server includes in its own
/// listing, and for anything whose href leaves `dir`: a server that answers a
/// listing of `/a/` with `/b/secret` is either broken or trying something, and
/// following it would put a row in the panel that is not where the panel says
/// it is.
#[must_use]
pub fn entry_of(response: &Response, dir: &str, origin: &str) -> Option<Entry> {
    // An href may be absolute (`http://host/a/b`) or a path (`/a/b`). Both are
    // legal and half the servers in the world use each.
    //
    // An absolute one is checked against the connection's **own** origin. A
    // server answering a listing with `http://elsewhere.invalid/share/file`
    // would otherwise put a row in the panel that points at another host
    // entirely, and every later operation on that row - open it, copy it,
    // delete it - would go to this server with that path, which is a
    // different file from the one the row appeared to name.
    let path = match response.href.find("://") {
        Some(at) => {
            let after = response.href.get(at + 3..)?;
            let slash = after.find('/')?;
            let host = after.get(..slash)?;
            let ours = origin.split("://").nth(1)?;
            // Compared without the port when the origin carries none, because
            // a server is free to write the default port back or leave it out.
            if host != ours && host.split(':').next()? != ours.split(':').next()? {
                return None;
            }
            after.get(slash..)?.to_string()
        }
        None => response.href.clone(),
    };
    let trimmed = path.trim_end_matches('/');
    let dir_trimmed = dir.trim_end_matches('/');
    if trimmed == dir_trimmed {
        return None;
    }
    let rest = path.strip_prefix(dir)?;
    let name = rest.trim_matches('/');
    // One component. A deeper path means the server answered a `Depth: 1` with
    // a whole tree, and the rows below the first level are not this listing.
    if name.is_empty() || name.contains('/') {
        return None;
    }
    let mut entry = if response.is_dir {
        Entry::dir(name)
    } else {
        Entry::file(name)
    };
    entry.size = if response.is_dir { 0 } else { response.len };
    entry.kind = if response.is_dir {
        EntryKind::Dir
    } else {
        EntryKind::File
    };
    entry.mtime = response.modified.as_deref().and_then(http_date);
    Some(entry)
}

/// An RFC 1123 date, which is what `getlastmodified` is defined to be.
///
/// `None` for anything else, including the ISO 8601 some servers send instead:
/// a wrong date is worse than none, and the panel already draws an empty date
/// cell for a backend that reports no time.
fn http_date(text: &str) -> Option<std::time::SystemTime> {
    // `Tue, 15 Nov 1994 12:45:26 GMT`
    let text = text.trim();
    let rest = text.split_once(", ")?.1;
    let mut parts = rest.split_whitespace();
    let day: u32 = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i32 = parts.next()?.parse().ok()?;
    let mut hms = parts.next()?.split(':');
    let hour: u32 = hms.next()?.parse().ok()?;
    let minute: u32 = hms.next()?.parse().ok()?;
    let second: u32 = hms.next()?.parse().ok()?;
    let stamp = chrono::NaiveDate::from_ymd_opt(year, month, day)?
        .and_hms_opt(hour, minute, second)?
        .and_utc()
        .timestamp();
    let secs = u64::try_from(stamp).ok()?;
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
}
