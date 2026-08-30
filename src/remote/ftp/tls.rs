//! Where an FTPS connection's trust comes from.
//!
//! One `rustls` configuration per session, built from the system CA bundle
//! this machine actually has: the bundle's location differs by distribution
//! and there is no portable way to ask, so the known locations are tried in
//! order and the first that reads is the answer.
//!
//! The PEM is parsed here rather than pulled in as a dependency, because what
//! is needed is one thing: the DER inside each `CERTIFICATE` block. A
//! certificate that will not decode is skipped rather than failing the
//! bundle, since a bundle with one malformed entry is still a bundle.

use super::*;

/// The TLS configuration FTPS uses, with the system's certificates and no way
/// to turn verification off.
///
/// There is no `insecure` key and no accept-any-certificate path, for the
/// reason the design gives about host keys: an override that exists is an
/// override that gets used. A machine with no certificate store is an error
/// that says which files were looked for, never a silent downgrade.
///
/// The roots are read from the system bundle rather than from a vendored copy,
/// so that a machine's own trust decisions (an added corporate root, a removed
/// one) are the ones that apply. `webpki-roots` would be a fifth crate and
/// the design does not list one.
pub(super) fn tls_config() -> Result<Arc<ClientConfig>> {
    let (path, pem) = read_ca_bundle()?;
    let mut roots = RootCertStore::empty();
    let certs: Vec<CertificateDer<'static>> = pem_certificates(&pem)
        .into_iter()
        .map(CertificateDer::from)
        .collect();
    let (valid, _) = roots.add_parsable_certificates(certs);
    if valid == 0 {
        return Err(Error::msg(format!(
            "{}: no usable certificate was found, so an FTPS server cannot be verified",
            path.display()
        )));
    }
    // `ClientConfig::builder` resolves the provider from the crate features
    // and documents that it panics when it cannot; naming the provider here
    // means there is nothing left for it to fail to resolve, which is the
    // house rule about panic paths applied to somebody else's constructor.
    let config = ClientConfig::builder_with_provider(Arc::new(
        suppaftp::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|err| Error::msg(format!("TLS could not be configured: {err}")))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Where a Linux distribution keeps its certificate bundle.
///
/// `SSL_CERT_FILE` first, because that is the variable every other TLS client
/// on the machine honours and a user who has set it means it.
const CA_BUNDLES: &[&str] = &[
    // Debian, Ubuntu, Arch, Gentoo.
    "/etc/ssl/certs/ca-certificates.crt",
    // Fedora, RHEL.
    "/etc/pki/tls/certs/ca-bundle.crt",
    // openSUSE.
    "/etc/ssl/ca-bundle.pem",
    // Alpine, and the BSDs.
    "/etc/ssl/cert.pem",
];

/// Read the first certificate bundle that exists, or say what was looked for.
fn read_ca_bundle() -> Result<(std::path::PathBuf, String)> {
    if let Some(from_env) = std::env::var_os("SSL_CERT_FILE") {
        let path = std::path::PathBuf::from(from_env);
        let text = std::fs::read_to_string(&path).map_err(|err| Error::io(&path, err))?;
        return Ok((path, text));
    }
    for candidate in CA_BUNDLES {
        let path = Path::new(candidate);
        if let Ok(text) = std::fs::read_to_string(path) {
            return Ok((path.to_path_buf(), text));
        }
    }
    Err(Error::msg(format!(
        "no system certificate bundle was found ({}), so an FTPS server cannot be verified; \
         set SSL_CERT_FILE to one",
        CA_BUNDLES.join(", ")
    )))
}

/// Every `CERTIFICATE` block of a PEM file, as DER.
///
/// Hand-written because `rustls-pki-types` is built here without its `pem`
/// feature (it arrives as `rustls`'s own dependency) and adding a PEM crate
/// would be adding a crate the design does not list. It is twenty lines of
/// base64 and it is tested against a fixture.
pub(super) fn pem_certificates(pem: &str) -> Vec<Vec<u8>> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let mut out = Vec::new();
    let mut body: Option<String> = None;
    for line in pem.lines() {
        let line = line.trim();
        if line == BEGIN {
            body = Some(String::new());
        } else if line == END {
            if let Some(text) = body.take()
                && let Some(der) = base64_decode(&text)
            {
                out.push(der);
            }
        } else if let Some(text) = body.as_mut() {
            text.push_str(line);
        }
    }
    out
}

/// Standard base64, no line breaks left in it, `=` padding tolerated.
///
/// `None` for anything that is not base64, which is what makes a truncated
/// bundle a skipped certificate rather than a wrong one.
pub(super) fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => u32::from(byte - b'A'),
            b'a'..=b'z' => u32::from(byte - b'a') + 26,
            b'0'..=b'9' => u32::from(byte - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b' ' | b'\t' | b'\r' | b'\n' => continue,
            _ => return None,
        };
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            let shifted = acc >> bits;
            out.push(u8::try_from(shifted & 0xff).ok()?);
        }
    }
    Some(out)
}
