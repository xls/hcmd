//! Just enough XML for two backends, and no more.
//!
//! WebDAV's `multistatus` and S3's `ListObjectsV2` are both small, flat
//! documents, and both are read by matching on **local names with the prefix
//! discarded**. That is the awkward part of each: WebDAV servers spell the DAV
//! namespace `D:`, `d:`, `lp1:` or nothing at all, and S3 endpoints differ
//! about whether they namespace at all. A parser that cared would need
//! configuring per server.
//!
//! Everything here is read-only over `&str` and allocates only what it
//! returns. Neither caller trusts the document: both bound what they take out
//! of it, because the length of a reply is the server's to choose.

/// The local name of an element, prefix discarded.
pub fn local(name: &str) -> &str {
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

/// The text of the first `<name>` element, prefix-insensitive.
pub fn first_text(xml: &str, name: &str) -> Option<String> {
    first_element(xml, name).map(|inner| unescape(&inner))
}

/// The inner text of the first element with this local name.
pub fn first_element(xml: &str, name: &str) -> Option<String> {
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
pub fn split_elements(xml: &str, name: &str) -> Vec<String> {
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
pub fn split_tags(fragment: &str) -> impl Iterator<Item = String> + '_ {
    fragment.split('<').filter_map(|piece| {
        let tag = piece.split('>').next()?;
        let bare = tag.trim().trim_end_matches('/').split_whitespace().next()?;
        (!bare.is_empty() && !bare.starts_with('/') && !bare.starts_with('?'))
            .then(|| bare.to_string())
    })
}

/// The five XML entities, which is all a `getlastmodified` or an `href` can
/// legally contain.
pub fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}
