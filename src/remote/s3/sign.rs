//! AWS Signature Version 4.
//!
//! The one part of S3 that has to be exactly right and gives no clue when it
//! is not: a wrong signature is a `403` with an opaque message, and every
//! difference - a header out of order, a path encoded twice, a date off by a
//! minute - produces the same answer. So it is written against the
//! specification's own worked example and tested against it, which is the only
//! way to know it is right before pointing it at a real endpoint.
//!
//! # The shape
//!
//! Four steps, in this order:
//!
//! 1. a **canonical request**: method, URI, query, headers, signed-header
//!    list, and the payload hash;
//! 2. a **string to sign**: the algorithm, the timestamp, the scope, and the
//!    hash of step 1;
//! 3. a **signing key**, derived from the secret by four chained HMACs, one
//!    per scope component;
//! 4. the **signature**, an HMAC of step 2 under step 3.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

/// SHA-256 of an empty body, which every GET and DELETE carries.
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Lowercase hex of a SHA-256.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    Digest::update(&mut hasher, bytes);
    hex(&Digest::finalize(hasher))
}

/// Lowercase hex.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// HMAC-SHA256.
fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    // HMAC accepts a key of any length, so this cannot fail; the fallback is
    // an empty key rather than a panic, because a signing path that can abort
    // the process is worse than one that produces a signature the server
    // rejects.
    let Ok(mut mac) = <Hmac<Sha256> as KeyInit>::new_from_slice(key) else {
        return Vec::new();
    };
    Mac::update(&mut mac, data);
    Mac::finalize(mac).into_bytes().to_vec()
}

/// What a request needs signing.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    /// `GET`, `PUT`, `DELETE`.
    pub method: &'a str,
    /// The path, already percent-encoded, beginning with `/`.
    pub uri: &'a str,
    /// The query string, already canonical: sorted, encoded, no leading `?`.
    pub query: &'a str,
    /// `host` without the scheme.
    pub host: &'a str,
    /// Lowercase hex of the payload's SHA-256.
    pub payload_hash: &'a str,
    /// `20130524T000000Z`.
    pub timestamp: &'a str,
    /// Extra headers to sign, lowercase names, already sorted by the caller.
    pub extra: &'a [(String, String)],
}

/// What signing produces: the headers to add.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signed {
    /// The `Authorization` header's value.
    pub authorization: String,
    /// The canonical request, kept because a mismatch is otherwise unreadable
    /// and this is what an endpoint's error message quotes back.
    pub canonical: String,
}

/// Sign one request.
///
/// `timestamp` is `YYYYMMDDTHHMMSSZ` and its date half is the scope's, so the
/// two cannot disagree - which they can when a caller formats each separately
/// and the clock crosses midnight between them.
#[must_use]
pub fn sign(request: &Request<'_>, access_key: &str, secret: &[u8], region: &str) -> Signed {
    let date = request.timestamp.get(..8).unwrap_or(request.timestamp);
    let scope = format!("{date}/{region}/s3/aws4_request");

    // Canonical headers: host, the payload hash, the date, and whatever the
    // caller added. Sorted by name, values trimmed, each line `name:value\n`.
    let mut headers: Vec<(String, String)> = vec![
        ("host".to_string(), request.host.to_string()),
        (
            "x-amz-content-sha256".to_string(),
            request.payload_hash.to_string(),
        ),
        ("x-amz-date".to_string(), request.timestamp.to_string()),
    ];
    headers.extend(request.extra.iter().cloned());
    headers.sort_by(|a, b| a.0.cmp(&b.0));
    let canonical_headers: String = headers
        .iter()
        .map(|(name, value)| format!("{name}:{}\n", value.trim()))
        .collect();
    let signed_headers: Vec<String> = headers.iter().map(|(name, _)| name.clone()).collect();
    let signed_headers = signed_headers.join(";");

    let canonical = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{}",
        request.method, request.uri, request.query, request.payload_hash
    );

    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        request.timestamp,
        sha256_hex(canonical.as_bytes())
    );

    // The signing key: four chained HMACs, one per scope component. Derived
    // per request rather than cached, because it is cheap and a cached one is
    // a secret with a lifetime to get wrong.
    let mut key = Vec::with_capacity(4 + secret.len());
    key.extend_from_slice(b"AWS4");
    key.extend_from_slice(secret);
    let key = hmac(&key, date.as_bytes());
    let key = hmac(&key, region.as_bytes());
    let key = hmac(&key, b"s3");
    let key = hmac(&key, b"aws4_request");
    let signature = hex(&hmac(&key, to_sign.as_bytes()));

    Signed {
        authorization: format!(
            "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, \
             SignedHeaders={signed_headers}, Signature={signature}"
        ),
        canonical,
    }
}

/// Percent-encode one path segment, S3's way.
///
/// Everything but the unreserved set, and **uppercase hex**: a lowercase `%2f`
/// hashes differently from `%2F` and the signature is over the encoded form,
/// so this is not cosmetic.
#[must_use]
pub fn encode_segment(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'-' | b'_' | b'.' | b'~' => out.push(char::from(byte)),
            b if b.is_ascii_alphanumeric() => out.push(char::from(b)),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Encode a whole key, keeping `/` as a separator.
#[must_use]
pub fn encode_key(key: &str) -> String {
    key.split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// A canonical query string: sorted by name, both halves encoded.
#[must_use]
pub fn canonical_query(pairs: &[(&str, String)]) -> String {
    let mut encoded: Vec<String> = pairs
        .iter()
        .map(|(name, value)| format!("{}={}", encode_segment(name), encode_segment(value)))
        .collect();
    encoded.sort();
    encoded.join("&")
}

#[cfg(test)]
#[path = "sign_tests.rs"]
mod tests;
