//! The WebDAV the share speaks: enough for a client to browse it.
//!
//! A directory index in HTML is for a person with a browser; a program
//! wanting the listing has no standard way to read that. WebDAV is the
//! standard, and hcmd already speaks it as a client, so the share answers
//! `OPTIONS` and `PROPFIND` - the two requests a read-only WebDAV client
//! needs to list and then `GET`. Nothing that writes is offered: `PUT`,
//! `DELETE`, `MKCOL` and `MOVE` are refused with the methods that are allowed.

use std::time::SystemTime;

use super::http::{html_escape, http_date};

/// The methods this share answers, as the `Allow` and `DAV` headers say.
pub const ALLOW: &str = "OPTIONS, GET, HEAD, PROPFIND";

/// One resource in a `PROPFIND` answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DavResource {
    /// The href, absolute on this server and already percent-encoded; a
    /// collection's ends in `/`.
    pub href: String,
    /// What it is called.
    pub name: String,
    /// A collection (directory) rather than a file.
    pub is_dir: bool,
    /// Size in bytes, for a file.
    pub size: u64,
    /// When it was last changed, when known.
    pub modified: Option<SystemTime>,
    /// The content type, for a file.
    pub content_type: String,
}

/// The `Depth` a `PROPFIND` asks for: the resource itself, or it and its
/// children. `infinity` is read as `1` - walking a whole share for one
/// request is not something a read-only share owes anybody.
#[must_use]
pub fn depth(header: Option<&str>) -> u8 {
    match header.map(str::trim) {
        Some("0") => 0,
        _ => 1,
    }
}

/// The `207 Multi-Status` body listing `resources`.
///
/// Every property a browsing client reads: name, collection-or-not, size,
/// last-modified and content type. Anything else asked for is simply not in
/// the answer, which the protocol allows.
#[must_use]
pub fn multistatus(resources: &[DavResource]) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"DAV:\">\n",
    );
    for r in resources {
        out.push_str("<D:response>\n<D:href>");
        out.push_str(&html_escape(&r.href));
        out.push_str("</D:href>\n<D:propstat>\n<D:prop>\n<D:displayname>");
        out.push_str(&html_escape(&r.name));
        out.push_str("</D:displayname>\n");
        if r.is_dir {
            out.push_str("<D:resourcetype><D:collection/></D:resourcetype>\n");
        } else {
            out.push_str("<D:resourcetype/>\n");
            out.push_str(&format!(
                "<D:getcontentlength>{}</D:getcontentlength>\n",
                r.size
            ));
            out.push_str(&format!(
                "<D:getcontenttype>{}</D:getcontenttype>\n",
                html_escape(&r.content_type)
            ));
        }
        if let Some(at) = r.modified {
            out.push_str(&format!(
                "<D:getlastmodified>{}</D:getlastmodified>\n",
                http_date(at)
            ));
        }
        out.push_str(
            "</D:prop>\n<D:status>HTTP/1.1 200 OK</D:status>\n</D:propstat>\n</D:response>\n",
        );
    }
    out.push_str("</D:multistatus>\n");
    out
}

#[cfg(test)]
#[path = "dav_tests.rs"]
mod tests;
