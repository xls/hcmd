use super::*;
use std::time::Duration;

#[test]
fn a_request_head_is_parsed_and_its_headers_are_case_insensitive() {
    let raw = b"GET /a%20b/c.txt?x=1 HTTP/1.1\r\nHost: h\r\nRange: bytes=0-9\r\n\r\nbody";
    let req = parse_request(raw).expect("a whole head");
    assert_eq!(req.method, "GET");
    assert_eq!(req.path, "/a b/c.txt", "decoded, and the query dropped");
    assert_eq!(req.header("range"), Some("bytes=0-9"));
    assert_eq!(req.header("RANGE"), Some("bytes=0-9"));
    assert_eq!(req.header("host"), Some("h"));
    assert_eq!(head_end(raw), Some(raw.len() - 4));
}

#[test]
fn an_incomplete_malformed_or_oversized_head_is_refused_for_the_right_reason() {
    assert_eq!(
        parse_request(b"GET / HTTP/1.1\r\nHost: h\r\n"),
        Err(ParseError::Incomplete)
    );
    assert_eq!(
        parse_request(b"nonsense\r\n\r\n"),
        Err(ParseError::Malformed)
    );
    assert_eq!(
        parse_request(b"GET relative HTTP/1.1\r\n\r\n"),
        Err(ParseError::Malformed)
    );
    assert_eq!(
        parse_request(b"GET / SMTP/1.0\r\n\r\n"),
        Err(ParseError::Malformed)
    );
    let huge = vec![b'x'; MAX_HEAD + 1];
    assert_eq!(parse_request(&huge), Err(ParseError::TooLarge));
}

#[test]
fn percent_coding_round_trips_a_name_with_spaces_and_unicode() {
    let name = "my file (2) ü&?.txt";
    let encoded = percent_encode(name);
    assert!(!encoded.contains(' '), "{encoded}");
    assert!(!encoded.contains('&'), "{encoded}");
    assert!(!encoded.contains('?'), "{encoded}");
    assert_eq!(percent_decode(&encoded), name);
    // A stray `%` that is not an escape is kept as itself.
    assert_eq!(percent_decode("100%"), "100%");
}

#[test]
fn ranges_resolve_against_the_length_or_say_they_cannot() {
    assert_eq!(resolve_range(None, 100), None);
    assert_eq!(resolve_range(Some("bytes=0-9"), 100), Some(Ok((0, 9))));
    assert_eq!(resolve_range(Some("bytes=90-"), 100), Some(Ok((90, 99))));
    assert_eq!(resolve_range(Some("bytes=-10"), 100), Some(Ok((90, 99))));
    // Past the end is clipped; entirely past it cannot be satisfied.
    assert_eq!(resolve_range(Some("bytes=50-500"), 100), Some(Ok((50, 99))));
    assert_eq!(resolve_range(Some("bytes=100-"), 100), Some(Err(())));
    assert_eq!(resolve_range(Some("bytes=5-2"), 100), Some(Err(())));
    // Only the first of several ranges is honoured.
    assert_eq!(resolve_range(Some("bytes=0-1, 5-6"), 100), Some(Ok((0, 1))));
    // Not bytes: ignored, so the whole file goes.
    assert_eq!(resolve_range(Some("lines=1-2"), 100), None);
}

#[test]
fn a_response_head_is_a_status_line_and_headers_ending_in_a_blank_line() {
    let r = Response::new(206).header("Content-Range", "bytes 0-9/100");
    let head = String::from_utf8(r.head()).expect("ascii");
    assert!(
        head.starts_with("HTTP/1.1 206 Partial Content\r\n"),
        "{head}"
    );
    assert!(
        head.contains("\r\nContent-Range: bytes 0-9/100\r\n"),
        "{head}"
    );
    assert!(head.contains("\r\nAccept-Ranges: bytes\r\n"), "{head}");
    assert!(head.ends_with("\r\n\r\n"), "{head}");
    // A HEAD keeps the length and drops the body.
    let r = Response::text(404, "no such file").without_body();
    assert_eq!(r.body, Body::Empty);
    assert!(
        r.headers
            .iter()
            .any(|(n, v)| n == "Content-Length" && v == "13")
    );
}

#[test]
fn the_content_type_comes_from_the_name_and_text_says_its_charset() {
    assert_eq!(content_type(Path::new("a.png")), "image/png");
    assert_eq!(
        content_type(Path::new("a.txt")),
        "text/plain; charset=utf-8"
    );
    assert_eq!(content_type(Path::new("a.zip")), "application/zip");
    assert_eq!(content_type(Path::new("noext")), "application/octet-stream");
}

#[test]
fn dates_and_sizes_print_the_way_a_browser_and_a_reader_expect() {
    let at = SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs(784_111_777))
        .expect("epoch + n");
    assert_eq!(http_date(at), "Sun, 06 Nov 1994 08:49:37 GMT");
    assert_eq!(human_size(812), "812 B");
    assert_eq!(human_size(1_258_291), "1.2 MB");
    assert_eq!(human_size(1024), "1.0 KB");
}

#[test]
fn the_index_lists_folders_first_with_a_parent_link_and_escapes_everything() {
    let entries = vec![
        IndexEntry {
            name: "zeta.txt".into(),
            href: "zeta.txt".into(),
            is_dir: false,
            size: 10,
            modified: None,
        },
        IndexEntry {
            name: "a<b>&\"c".into(),
            href: "a%3Cb%3E%26%22c/".into(),
            is_dir: true,
            size: 0,
            modified: None,
        },
    ];
    let page = index_html("Shared", Some("../"), &entries);
    let folder = page
        .find("a&lt;b&gt;&amp;&quot;c/")
        .expect("the folder, escaped");
    let file = page.find("zeta.txt").expect("the file");
    assert!(folder < file, "folders first:\n{page}");
    assert!(page.contains("href=\"../\">..</a>"), "{page}");
    assert!(page.contains("10 B"), "{page}");
    assert!(!page.contains("<script"), "no script, ever");
    assert!(page.contains("<title>Shared</title>"));
    // No parent at the root.
    assert!(!index_html("Shared", None, &entries).contains(">..</a>"));
}
