//! S3: splitting paths, reading listings, and choosing a region.

use super::*;
use crate::remote::s3::list::{buckets, objects};

/// A `ListObjectsV2` reply as an endpoint writes one.
const LISTING: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>photos</Name>
  <Prefix>2024/</Prefix>
  <Delimiter>/</Delimiter>
  <IsTruncated>false</IsTruncated>
  <Contents>
    <Key>2024/beach.jpg</Key>
    <LastModified>2024-05-24T00:00:00.000Z</LastModified>
    <Size>184320</Size>
  </Contents>
  <Contents>
    <Key>2024/two words.jpg</Key>
    <LastModified>2024-05-24T00:00:00.000Z</LastModified>
    <Size>7</Size>
  </Contents>
  <CommonPrefixes><Prefix>2024/raw/</Prefix></CommonPrefixes>
</ListBucketResult>"#;

#[test]
fn a_listing_becomes_files_and_the_prefixes_below_it() {
    let page = objects(LISTING, "2024/", MAX_KEYS);
    let names: Vec<&str> = page.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["raw", "beach.jpg", "two words.jpg"]);
    let raw = &page.entries[0];
    assert!(raw.is_dir(), "a CommonPrefix is a directory");
    let beach = &page.entries[1];
    assert!(!beach.is_dir());
    assert_eq!(beach.size, 184_320);
    assert!(beach.mtime.is_some(), "an ISO 8601 LastModified parses");
    assert_eq!(page.next, None, "the reply was not truncated");
}

#[test]
fn a_truncated_reply_carries_the_token_to_continue_with() {
    let xml = r#"<ListBucketResult>
      <IsTruncated>true</IsTruncated>
      <NextContinuationToken>opaque-token</NextContinuationToken>
      <Contents><Key>a.txt</Key><Size>1</Size></Contents>
    </ListBucketResult>"#;
    let page = objects(xml, "", MAX_KEYS);
    assert_eq!(page.next.as_deref(), Some("opaque-token"));
    // And a reply that says nothing about truncation ends the walk, rather
    // than looping for ever on a missing token.
    let once = objects(
        "<ListBucketResult><Contents><Key>a</Key></Contents></ListBucketResult>",
        "",
        MAX_KEYS,
    );
    assert_eq!(once.next, None);
}

#[test]
fn the_prefix_marker_object_is_not_a_file_inside_itself() {
    // A zero-byte object at `2024/` is how an empty directory is made. It is
    // that directory, not a nameless file in it.
    let xml = r#"<ListBucketResult>
      <Contents><Key>2024/</Key><Size>0</Size></Contents>
      <Contents><Key>2024/a.txt</Key><Size>3</Size></Contents>
    </ListBucketResult>"#;
    let page = objects(xml, "2024/", MAX_KEYS);
    let names: Vec<&str> = page.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["a.txt"]);
}

#[test]
fn a_key_that_is_not_below_the_prefix_is_dropped() {
    // The panel is showing that prefix. A row from somewhere else would be
    // drawn at a path it does not occupy, and every later operation on it
    // would go somewhere the user did not point.
    let xml = r#"<ListBucketResult>
      <Contents><Key>elsewhere/secret.txt</Key><Size>1</Size></Contents>
      <Contents><Key>2024/ok.txt</Key><Size>1</Size></Contents>
    </ListBucketResult>"#;
    let page = objects(xml, "2024/", MAX_KEYS);
    let names: Vec<&str> = page.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["ok.txt"]);
}

#[test]
fn a_deeper_key_is_not_a_row_in_this_listing() {
    // With `delimiter=/` the endpoint should not send these, but the listing
    // is the server's to compose and a name with a slash in it is not a row.
    let xml = r#"<ListBucketResult>
      <Contents><Key>2024/raw/deep.dng</Key><Size>1</Size></Contents>
    </ListBucketResult>"#;
    assert!(objects(xml, "2024/", MAX_KEYS).entries.is_empty());
}

#[test]
fn a_listing_is_bounded_however_much_the_endpoint_sends() {
    let one = "<Contents><Key>f.txt</Key><Size>1</Size></Contents>";
    let xml = format!("<ListBucketResult>{}</ListBucketResult>", one.repeat(50));
    assert_eq!(objects(&xml, "", 10).entries.len(), 10);
}

#[test]
fn the_root_of_a_connection_lists_buckets() {
    let xml = r#"<ListAllMyBucketsResult><Buckets>
      <Bucket><Name>photos</Name><CreationDate>2024-05-24T00:00:00.000Z</CreationDate></Bucket>
      <Bucket><Name>backups</Name><CreationDate>2024-05-24T00:00:00.000Z</CreationDate></Bucket>
    </Buckets></ListAllMyBucketsResult>"#;
    let rows = buckets(xml, MAX_KEYS);
    let names: Vec<&str> = rows.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["photos", "backups"]);
    assert!(
        rows.iter().all(crate::vfs::Entry::is_dir),
        "a bucket is a directory"
    );
}

#[test]
fn a_path_splits_into_a_bucket_and_a_key() {
    assert_eq!(
        S3Fs::split("/photos/2024/a.jpg"),
        (Some("photos".to_string()), "2024/a.jpg".to_string())
    );
    assert_eq!(
        S3Fs::split("/photos"),
        (Some("photos".to_string()), String::new())
    );
    assert_eq!(
        S3Fs::split("/photos/"),
        (Some("photos".to_string()), String::new())
    );
    // The root is no bucket at all, which is the listing that names buckets.
    assert_eq!(S3Fs::split("/"), (None, String::new()));
    assert_eq!(S3Fs::split(""), (None, String::new()));
}

#[test]
fn the_region_comes_from_an_aws_hostname_and_nowhere_else() {
    assert_eq!(region_of("s3.eu-west-1.amazonaws.com"), "eu-west-1");
    assert_eq!(region_of("s3.us-gov-east-1.amazonaws.com"), "us-gov-east-1");
    // `s3.amazonaws.com` names no region, and us-east-1 is what it is.
    assert_eq!(region_of("s3.amazonaws.com"), "us-east-1");
    // Everything else: MinIO, Ceph, R2. They verify the signature against
    // whatever region it claims, so the value only has to match itself.
    assert_eq!(region_of("minio.example.invalid"), "us-east-1");
    assert_eq!(region_of("localhost"), "us-east-1");
}
