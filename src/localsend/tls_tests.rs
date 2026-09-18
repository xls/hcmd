//! The fingerprint pin, without a network.

use super::*;

/// A verifier over `expected`, and the slot it records into.
fn pin(expected: Option<&str>) -> Pin {
    Pin {
        expected: expected.map(str::to_string),
        seen: Mutex::new(None),
        algorithms: provider().signature_verification_algorithms,
    }
}

/// `verify_server_cert` on `der`, with the arguments the pin ignores.
fn verify(pin: &Pin, der: &[u8]) -> Result<ServerCertVerified, rustls::Error> {
    let cert = CertificateDer::from(der.to_vec());
    let name = ServerName::try_from("192.168.1.9".to_string()).expect("an address");
    pin.verify_server_cert(&cert, &[], &name, &[], UnixTime::now())
}

#[test]
fn the_fingerprint_is_the_lower_case_hex_sha256_of_the_der() {
    // SHA-256("abc"), a known answer.
    assert_eq!(
        fingerprint_of(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn a_certificate_matching_the_announced_fingerprint_is_accepted_case_insensitively() {
    let der = b"pretend certificate bytes";
    let expected = fingerprint_of(der).to_uppercase();
    let pin = pin(Some(&expected));
    assert!(verify(&pin, der).is_ok());
    assert_eq!(
        pin.seen.lock().expect("lock").as_deref(),
        Some(fingerprint_of(der).as_str()),
        "and what was seen is recorded"
    );
}

#[test]
fn a_certificate_that_is_not_the_announced_one_is_refused_by_name() {
    let pin = pin(Some(&fingerprint_of(b"the certificate it announced")));
    let err = verify(&pin, b"some other certificate").expect_err("refused");
    let text = err.to_string();
    assert!(text.contains("not the one it announced"), "{text}");
    assert!(pin.seen.lock().expect("lock").is_none(), "nothing recorded");
}

#[test]
fn a_typed_address_announced_nothing_so_anything_is_accepted_and_recorded() {
    let pin = pin(None);
    assert!(verify(&pin, b"whatever it presents").is_ok());
    assert_eq!(
        pin.seen.lock().expect("lock").as_deref(),
        Some(fingerprint_of(b"whatever it presents").as_str())
    );
}

#[test]
fn the_pin_still_offers_the_providers_signature_schemes() {
    let pin = pin(None);
    assert!(!pin.supported_verify_schemes().is_empty());
}

#[test]
fn a_closed_port_is_a_connect_error_not_a_hang() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    let Err(err) = connect_plain("127.0.0.1", port) else {
        panic!("nothing listens there");
    };
    assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused);
}

/// Not a test: a probe of the real app's HTTP answers. Run by hand:
/// `HCMD_LS_PROBE=host:port cargo test --lib -- --ignored --nocapture probe_real_info`.
#[test]
#[ignore = "a manual probe of a real device"]
fn probe_real_info() {
    let Ok(target) = std::env::var("HCMD_LS_PROBE") else {
        return;
    };
    let (host, port) = target.rsplit_once(':').expect("host:port");
    let port: u16 = port.parse().expect("port");
    let mut connection = connect_tls(host, port, None).expect("tls");
    eprintln!("fingerprint seen: {:?}", connection.fingerprint);
    use std::io::{Read, Write};
    connection
        .stream
        .write_all(
            format!("GET /api/localsend/v2/info HTTP/1.1\r\nHost: {target}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
                .as_bytes(),
        )
        .expect("write");
    let mut out = Vec::new();
    let mut buf = [0_u8; 4096];
    let start = std::time::Instant::now();
    loop {
        match connection.stream.read(&mut buf) {
            Ok(0) => {
                eprintln!("closed by server after {:?}", start.elapsed());
                break;
            }
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if start.elapsed() > std::time::Duration::from_secs(3) {
                    eprintln!("still open after 3s; stopping");
                    break;
                }
            }
            Err(e) => {
                eprintln!("read error: {e} after {:?}", start.elapsed());
                break;
            }
        }
    }
    eprintln!("RAW>>>\n{}\n<<<RAW", String::from_utf8_lossy(&out));
    // And through the real client path: the same answer, parsed.
    let peer = crate::localsend::Peer::typed(&target);
    match crate::localsend::Sender::info(&peer) {
        Ok((device, seen)) => eprintln!(
            "Sender::info: alias={:?} version={} fingerprint announced={} seen={:?}",
            device.alias, device.version, device.fingerprint, seen
        ),
        Err(e) => eprintln!("Sender::info failed: {e}"),
    }
}
