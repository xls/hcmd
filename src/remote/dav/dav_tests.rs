//! WebDAV: the parsing, and what a hostile reply is not allowed to do.

use super::*;
use crate::remote::dav::parse::{Response, entry_of, multistatus, percent_decode};

/// A multistatus as Apache's mod_dav writes one, prefixes and all.
const APACHE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/share/</D:href>
    <D:propstat><D:prop>
      <D:resourcetype><D:collection/></D:resourcetype>
      <D:getlastmodified>Tue, 15 Nov 1994 12:45:26 GMT</D:getlastmodified>
    </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
  </D:response>
  <D:response>
    <D:href>/share/notes.txt</D:href>
    <D:propstat><D:prop>
      <D:resourcetype/>
      <D:getcontentlength>1234</D:getcontentlength>
      <D:getlastmodified>Tue, 15 Nov 1994 12:45:26 GMT</D:getlastmodified>
    </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
  </D:response>
  <D:response>
    <D:href>/share/sub/</D:href>
    <D:propstat><D:prop>
      <D:resourcetype><D:collection/></D:resourcetype>
    </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
  </D:response>
</D:multistatus>"#;

#[test]
fn it_reads_a_listing_and_leaves_the_directory_itself_out() {
    let rows = multistatus(APACHE, MAX_ENTRIES);
    assert_eq!(rows.len(), 3, "three responses");
    let entries: Vec<Entry> = rows
        .iter()
        .filter_map(|r| entry_of(r, "/share/", "http://dav.example.invalid"))
        .collect();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["notes.txt", "sub"],
        "every server includes the directory itself, and it is not a row in it"
    );
    assert!(!entries[0].is_dir(), "a resourcetype with nothing in it");
    assert!(entries[1].is_dir(), "a resourcetype holding a collection");
    assert_eq!(entries[0].size, 1234);
    assert!(entries[0].mtime.is_some(), "an RFC 1123 date parses");
}

#[test]
fn a_namespace_prefix_is_whatever_the_server_felt_like() {
    // The reason this parser matches on local names. Every one of these is a
    // real prefix a real server sends.
    for prefix in ["D:", "d:", "lp1:", ""] {
        let xml = format!(
            "<{p}multistatus><{p}response><{p}href>/a/b.txt</{p}href>\
             <{p}prop><{p}getcontentlength>7</{p}getcontentlength></{p}prop>\
             </{p}response></{p}multistatus>",
            p = prefix
        );
        let rows = multistatus(&xml, MAX_ENTRIES);
        assert_eq!(rows.len(), 1, "prefix {prefix:?}");
        assert_eq!(rows[0].len, 7, "prefix {prefix:?}");
    }
}

#[test]
fn a_percent_encoded_name_comes_back_as_it_was() {
    assert_eq!(percent_decode("/a/two%20words.txt"), "/a/two words.txt");
    assert_eq!(percent_decode("/a/caf%C3%A9"), "/a/café");
    // Malformed escapes are left alone rather than dropped: a literal `%` in
    // a filename is not an error.
    assert_eq!(percent_decode("/a/100%"), "/a/100%");
    assert_eq!(percent_decode("/a/%zz"), "/a/%zz");
}

#[test]
fn an_href_that_leaves_the_listing_is_refused() {
    // A server answering a listing of `/share/` with something outside it is
    // either broken or trying something. Following it would put a row in the
    // panel that is not where the panel says it is.
    for href in [
        "/etc/passwd",
        "/other/file.txt",
        "/share/../secret",
        "http://elsewhere.invalid/share/file.txt",
    ] {
        let row = Response {
            href: href.to_string(),
            is_dir: false,
            len: 0,
            modified: None,
        };
        assert!(
            entry_of(&row, "/share/", "http://dav.example.invalid").is_none(),
            "{href} should not become a row"
        );
    }
}

#[test]
fn a_deeper_path_is_not_a_row_in_this_listing() {
    // `Depth: 1` is asked for; a server that answers with a whole tree gets
    // its first level listed and the rest ignored, rather than a panel full
    // of rows whose names contain slashes.
    let row = Response {
        href: "/share/sub/deep.txt".to_string(),
        is_dir: false,
        len: 0,
        modified: None,
    };
    assert!(entry_of(&row, "/share/", "http://dav.example.invalid").is_none());
}

#[test]
fn an_absolute_href_on_this_server_is_accepted() {
    // Half the servers answer with a path and half with a full URL. Both are
    // legal and both mean the same file.
    let row = Response {
        href: "http://dav.example.invalid/share/notes.txt".to_string(),
        is_dir: false,
        len: 9,
        modified: None,
    };
    let entry = entry_of(&row, "/share/", "http://dav.example.invalid").expect("a row");
    assert_eq!(entry.name, "notes.txt");
}

#[test]
fn a_listing_is_bounded_however_much_the_server_sends() {
    let one = "<response><href>/a/f.txt</href></response>";
    let xml = format!("<multistatus>{}</multistatus>", one.repeat(50));
    assert_eq!(multistatus(&xml, 10).len(), 10, "the caller's bound holds");
}

#[test]
fn the_basic_header_is_the_credentials_and_nothing_else() {
    // RFC 7617's own example, which is the one thing about this that can be
    // checked against something other than itself.
    assert_eq!(
        basic_auth("Aladdin", b"open sesame"),
        "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
    );
    assert_eq!(base64(b""), "");
    assert_eq!(base64(b"f"), "Zg==");
    assert_eq!(base64(b"fo"), "Zm8=");
    assert_eq!(base64(b"foo"), "Zm9v");
    assert_eq!(base64(b"foob"), "Zm9vYg==");
}

#[test]
fn a_path_is_encoded_without_losing_its_separators() {
    assert_eq!(encode_path("/a/two words.txt"), "/a/two%20words.txt");
    assert_eq!(encode_path("/a/caf\u{e9}"), "/a/caf%C3%A9");
    assert_eq!(encode_path("/a/b_c-d.e~f"), "/a/b_c-d.e~f");
}

#[test]
fn a_directory_path_gets_exactly_one_trailing_slash() {
    assert_eq!(normalise_dir("/a/b"), "/a/b/");
    assert_eq!(normalise_dir("/a/b/"), "/a/b/");
    assert_eq!(normalise_dir("/a/b///"), "/a/b/");
    assert_eq!(normalise_dir(""), "/");
    assert_eq!(normalise_dir("/"), "/");
}
