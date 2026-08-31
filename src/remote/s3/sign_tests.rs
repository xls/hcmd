//! SigV4, against the specification's own worked example.
//!
//! These constants are AWS's published test vectors, not numbers this
//! implementation produced. That is the whole point: a signature that matches
//! what the specification says the answer is has been checked against
//! something other than itself, and a `403` from a real endpoint tells you
//! nothing about which of the four steps was wrong.

use super::*;

/// The example credentials from the SigV4 test suite.
const KEY: &str = "AKIAIOSFODNN7EXAMPLE";
const SECRET: &[u8] = b"wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

#[test]
fn the_empty_payload_hash_is_the_published_constant() {
    // Every GET and DELETE sends this, so it being wrong would break all of
    // them at once and none of them legibly.
    assert_eq!(sha256_hex(b""), EMPTY_SHA256);
}

#[test]
fn it_signs_the_specifications_own_example() {
    // GET on an object, from the S3 chapter of the SigV4 documentation.
    let signed = sign(
        &Request {
            method: "GET",
            uri: "/test.txt",
            query: "",
            host: "examplebucket.s3.amazonaws.com",
            payload_hash: EMPTY_SHA256,
            timestamp: "20130524T000000Z",
            extra: &[("range".to_string(), "bytes=0-9".to_string())],
        },
        KEY,
        SECRET,
        "us-east-1",
    );
    assert!(
        signed
            .authorization
            .contains("Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"),
        "{}",
        signed.authorization
    );
    // And the canonical request it was built from, which is what an endpoint
    // quotes back when it disagrees.
    assert!(
        signed.canonical.starts_with("GET\n/test.txt\n\n"),
        "{}",
        signed.canonical
    );
    assert!(
        signed
            .canonical
            .contains("host;range;x-amz-content-sha256;x-amz-date"),
        "the signed headers are sorted: {}",
        signed.canonical
    );
}

#[test]
fn it_signs_a_listing_with_a_query() {
    // The other shape every session uses: a GET on the bucket with query
    // parameters, which have to be sorted and encoded before they are signed.
    let query = canonical_query(&[
        ("list-type", "2".to_string()),
        ("prefix", "photos/2024/".to_string()),
        ("delimiter", "/".to_string()),
    ]);
    assert_eq!(
        query, "delimiter=%2F&list-type=2&prefix=photos%2F2024%2F",
        "sorted by name, both halves encoded, slashes escaped"
    );
    let signed = sign(
        &Request {
            method: "GET",
            uri: "/",
            query: &query,
            host: "examplebucket.s3.amazonaws.com",
            payload_hash: EMPTY_SHA256,
            timestamp: "20130524T000000Z",
            extra: &[],
        },
        KEY,
        SECRET,
        "us-east-1",
    );
    assert!(signed.canonical.contains(&query), "{}", signed.canonical);
}

#[test]
fn encoding_is_uppercase_hex_and_keeps_the_separators() {
    // A lowercase `%2f` hashes differently from `%2F`, and the signature is
    // over the encoded form, so this is not cosmetic.
    assert_eq!(encode_segment("a b"), "a%20b");
    assert_eq!(encode_segment("a/b"), "a%2Fb");
    assert_eq!(
        encode_key("photos/two words/a.jpg"),
        "photos/two%20words/a.jpg"
    );
    assert_eq!(encode_segment("caf\u{e9}"), "caf%C3%A9");
    assert_eq!(encode_segment("a-b_c.d~e"), "a-b_c.d~e");
}

#[test]
fn the_scope_date_comes_from_the_timestamp_it_was_signed_with() {
    // Two clocks formatted separately can disagree across midnight, and the
    // failure is a 403 an hour a day. The date is derived from the timestamp
    // so there is only one of them.
    let signed = sign(
        &Request {
            method: "GET",
            uri: "/",
            query: "",
            host: "h.example.invalid",
            payload_hash: EMPTY_SHA256,
            timestamp: "20240101T235959Z",
            extra: &[],
        },
        KEY,
        SECRET,
        "eu-west-1",
    );
    assert!(
        signed
            .authorization
            .contains("Credential=AKIAIOSFODNN7EXAMPLE/20240101/eu-west-1/s3/aws4_request"),
        "{}",
        signed.authorization
    );
}
