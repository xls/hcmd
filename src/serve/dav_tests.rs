use super::*;

#[test]
fn depth_is_zero_only_when_asked_and_infinity_is_read_as_one() {
    assert_eq!(depth(Some("0")), 0);
    assert_eq!(depth(Some(" 0 ")), 0);
    assert_eq!(depth(Some("1")), 1);
    assert_eq!(depth(Some("infinity")), 1);
    assert_eq!(depth(None), 1);
}

#[test]
fn a_multistatus_names_each_resource_with_the_properties_a_client_reads() {
    let at = std::time::SystemTime::UNIX_EPOCH
        .checked_add(std::time::Duration::from_secs(784_111_777))
        .expect("epoch + n");
    let body = multistatus(&[
        DavResource {
            href: "/share/".into(),
            name: "share".into(),
            is_dir: true,
            size: 0,
            modified: None,
            content_type: String::new(),
        },
        DavResource {
            href: "/share/a%26b.txt".into(),
            name: "a&b.txt".into(),
            is_dir: false,
            size: 42,
            modified: Some(at),
            content_type: "text/plain; charset=utf-8".into(),
        },
    ]);
    assert!(body.starts_with("<?xml version=\"1.0\""), "{body}");
    assert!(body.contains("<D:multistatus xmlns:D=\"DAV:\">"), "{body}");
    assert!(body.contains("<D:href>/share/</D:href>"), "{body}");
    assert!(
        body.contains("<D:resourcetype><D:collection/></D:resourcetype>"),
        "{body}"
    );
    // The file: escaped name, its size, type and date.
    assert!(body.contains("<D:href>/share/a%26b.txt</D:href>"), "{body}");
    assert!(
        body.contains("<D:displayname>a&amp;b.txt</D:displayname>"),
        "{body}"
    );
    assert!(
        body.contains("<D:getcontentlength>42</D:getcontentlength>"),
        "{body}"
    );
    assert!(
        body.contains("<D:getcontenttype>text/plain; charset=utf-8</D:getcontenttype>"),
        "{body}"
    );
    assert!(
        body.contains("<D:getlastmodified>Sun, 06 Nov 1994 08:49:37 GMT</D:getlastmodified>"),
        "{body}"
    );
    assert_eq!(body.matches("<D:response>").count(), 2);
    assert!(body.trim_end().ends_with("</D:multistatus>"));
}
