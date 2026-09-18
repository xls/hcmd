//! The TLS connection to a LocalSend device.
//!
//! A device's certificate is self-signed, so the web's chain of trust says
//! nothing about it. What the protocol offers instead is the *fingerprint*:
//! the SHA-256 of the certificate, which the device announces about itself.
//! So the verifier here pins that - a device whose certificate does not hash
//! to what it announced is refused - and for a device reached by a typed
//! address, which announced nothing, it records what it saw so the caller can
//! show it and remember it. Signatures in the handshake are still checked
//! properly; only the identity question is answered differently.

use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{
    CryptoProvider, WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature,
};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};
use sha2::{Digest, Sha256};

/// How long to wait for the TCP connect.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a read may wait. Generous on purpose: the answer to a
/// `prepare-upload` comes only when the person on the other side taps
/// Accept, which can take minutes. It is a ceiling so a device that goes
/// silent ends the job with an error rather than holding it forever.
const READ_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long a write may wait: a device that stops taking bytes mid-upload.
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);

/// The SHA-256 of a certificate as the protocol writes it: lower-case hex.
#[must_use]
pub fn fingerprint_of(cert: &[u8]) -> String {
    Sha256::digest(cert)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Pins the peer's certificate to an announced fingerprint, or records it.
#[derive(Debug)]
struct Pin {
    /// The fingerprint the device announced, when it announced one.
    expected: Option<String>,
    /// The fingerprint of the certificate actually presented.
    seen: Mutex<Option<String>>,
    /// The provider's signature algorithms, for the handshake checks.
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Pin {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let seen = fingerprint_of(end_entity.as_ref());
        if let Some(expected) = &self.expected
            && !expected.eq_ignore_ascii_case(&seen)
        {
            return Err(rustls::Error::General(format!(
                "the device's certificate ({}...) is not the one it announced ({}...)",
                seen.get(..12).unwrap_or(&seen),
                expected.get(..12).unwrap_or(expected)
            )));
        }
        if let Ok(mut slot) = self.seen.lock() {
            *slot = Some(seen);
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// A connection to a device, over TLS or not, and what was learned about it.
pub struct Connection {
    /// The stream to speak HTTP on.
    pub stream: Box<dyn Stream>,
    /// The fingerprint of the certificate the device presented, over TLS.
    pub fingerprint: Option<String>,
}

/// Read and write, on one object.
pub trait Stream: std::io::Read + std::io::Write + Send {}
impl<T: std::io::Read + std::io::Write + Send> Stream for T {}

/// Open a TCP connection to `host:port`.
fn tcp(host: &str, port: u16) -> std::io::Result<TcpStream> {
    use std::net::ToSocketAddrs;
    let mut last = std::io::Error::other(format!("{host}: no address"));
    for addr in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(stream) => {
                stream.set_nodelay(true)?;
                // A timeout surfaces through rustls's `StreamOwned` as the
                // io error it is, which is what a failed job wants.
                stream.set_read_timeout(Some(READ_TIMEOUT))?;
                stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
                return Ok(stream);
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Connect to `host:port` in the clear.
pub fn connect_plain(host: &str, port: u16) -> std::io::Result<Connection> {
    let stream = tcp(host, port)?;
    Ok(Connection {
        stream: Box::new(stream),
        fingerprint: None,
    })
}

/// Connect to `host:port` over TLS, pinning `expected` when there is one.
///
/// The handshake runs here, so a wrong certificate is this call's error and
/// not a later request's.
pub fn connect_tls(host: &str, port: u16, expected: Option<&str>) -> std::io::Result<Connection> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let pin = Arc::new(Pin {
        expected: expected.map(str::to_string),
        seen: Mutex::new(None),
        algorithms: provider.signature_verification_algorithms,
    });
    let config = ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(std::io::Error::other)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::clone(&pin) as Arc<dyn ServerCertVerifier>)
        .with_no_client_auth();
    let name = ServerName::try_from(host.to_string())
        .map_err(|_| std::io::Error::other(format!("{host}: not a host name or address")))?;
    let tcp = tcp(host, port)?;
    let mut tls = ClientConnection::new(Arc::new(config), name).map_err(std::io::Error::other)?;
    // Drive the handshake to completion before handing the stream over.
    let mut stream = StreamOwned::new(tls, tcp);
    while stream.conn.is_handshaking() {
        stream
            .conn
            .complete_io(&mut stream.sock)
            .map_err(|e| std::io::Error::other(format!("TLS handshake with {host}: {e}")))?;
    }
    tls = stream.conn;
    let sock = stream.sock;
    let fingerprint = pin.seen.lock().ok().and_then(|s| s.clone());
    Ok(Connection {
        stream: Box::new(StreamOwned::new(tls, sock)),
        fingerprint,
    })
}

/// The provider, exposed so a test can confirm it is the one ureq uses.
#[must_use]
pub fn provider() -> CryptoProvider {
    rustls::crypto::ring::default_provider()
}

#[cfg(test)]
#[path = "tls_tests.rs"]
mod tests;
