//! The sender against [`crate::localsend::fake`], the receiver on loopback
//! that speaks the protocol the way the specification says a receiver does.

use std::net::TcpListener;

use super::*;
use crate::localsend::fake::{Mode, Receiver, scratch};

fn me() -> DeviceInfo {
    DeviceInfo::ours("hcmd test", 1, Protocol::Http)
}

#[test]
fn a_folder_and_a_file_are_collected_with_relative_names_in_order() {
    let (dir, paths) = scratch("collect");
    let files = collect(&paths).expect("collect");
    let names: Vec<(&str, u64)> = files
        .iter()
        .map(|(m, _)| (m.file_name.as_str(), m.size))
        .collect();
    assert_eq!(
        names,
        vec![
            ("report.pdf", 9),
            ("album/2026/note.txt", 6),
            ("album/big.bin", 300_000)
        ]
    );
    let ids: std::collections::BTreeSet<&str> = files.iter().map(|(m, _)| m.id.as_str()).collect();
    assert_eq!(ids.len(), 3, "every file its own id");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_whole_send_arrives_byte_exact_with_lengths_and_the_receiver_hears_who_sent() {
    let (dir, paths) = scratch("happy");
    let receiver = Receiver::start(Mode::Accept);
    let peer = receiver.peer();
    let sender = Sender::new(me());
    let files = collect(&paths).expect("collect");
    let metas: Vec<FileMeta> = files.iter().map(|(m, _)| m.clone()).collect();

    let session = sender.prepare(&peer, &metas, None).expect("accepted");
    assert_eq!(session.id, "sess-1");
    assert_eq!(session.tokens.len(), 3);

    let mut progress_total = 0_u64;
    for (meta, path) in &files {
        Sender::upload(&peer, &session, meta, path, &mut |n| {
            progress_total += n;
            true
        })
        .expect("uploaded");
    }
    assert_eq!(progress_total, 300_000 + 9 + 6);

    let seen = receiver.seen.lock().expect("lock");
    assert_eq!(seen.prepared.len(), 1);
    assert_eq!(seen.prepared[0].info.alias, "hcmd test");
    assert_eq!(seen.host, format!("127.0.0.1:{}", receiver.port));
    let (len, bytes) = seen
        .uploaded
        .get("album/big.bin")
        .expect("the big one arrived");
    assert_eq!(*len, 300_000, "sent with a Content-Length");
    assert_eq!(
        bytes,
        &std::fs::read(dir.join("album/big.bin")).expect("original")
    );
    assert_eq!(
        seen.uploaded.get("report.pdf").map(|(_, b)| b.as_slice()),
        Some(&b"%PDF-1.4\n"[..])
    );
    assert_eq!(
        seen.uploaded
            .get("album/2026/note.txt")
            .map(|(_, b)| b.as_slice()),
        Some(&b"hello\n"[..])
    );
    drop(seen);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_pin_is_asked_for_and_then_given() {
    let (dir, paths) = scratch("pin");
    let receiver = Receiver::start(Mode::Pin);
    let peer = receiver.peer();
    let sender = Sender::new(me());
    let metas: Vec<FileMeta> = collect(&paths)
        .expect("collect")
        .into_iter()
        .map(|(m, _)| m)
        .collect();
    assert_eq!(
        sender.prepare(&peer, &metas, None),
        Err(SendError::PinRequired)
    );
    assert_eq!(
        sender.prepare(&peer, &metas, Some("0000")),
        Err(SendError::PinRequired),
        "a wrong one"
    );
    let session = sender
        .prepare(&peer, &metas, Some("1234"))
        .expect("the right one");
    assert_eq!(session.tokens.len(), 3);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_decline_and_a_busy_device_are_named_not_numbered() {
    let (dir, paths) = scratch("no");
    let metas: Vec<FileMeta> = collect(&paths)
        .expect("collect")
        .into_iter()
        .map(|(m, _)| m)
        .collect();
    let sender = Sender::new(me());
    let receiver = Receiver::start(Mode::Decline);
    assert_eq!(
        sender.prepare(&receiver.peer(), &metas, None),
        Err(SendError::Declined)
    );
    assert_eq!(SendError::Declined.to_string(), "the device declined");
    drop(receiver);
    let receiver = Receiver::start(Mode::Busy);
    assert_eq!(
        sender.prepare(&receiver.peer(), &metas, None),
        Err(SendError::Busy)
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn cancelling_mid_file_stops_the_upload_and_tells_the_device() {
    let (dir, paths) = scratch("cancel");
    let receiver = Receiver::start(Mode::Accept);
    let peer = receiver.peer();
    let sender = Sender::new(me());
    let files = collect(&paths).expect("collect");
    let metas: Vec<FileMeta> = files.iter().map(|(m, _)| m.clone()).collect();
    let session = sender.prepare(&peer, &metas, None).expect("accepted");
    let (big, big_path) = files
        .iter()
        .find(|(m, _)| m.file_name == "album/big.bin")
        .expect("big");
    let mut chunks = 0;
    let err = Sender::upload(&peer, &session, big, big_path, &mut |_| {
        chunks += 1;
        chunks < 2
    })
    .expect_err("cancelled");
    assert_eq!(err, SendError::Cancelled);
    Sender::cancel(&peer, &session).expect("cancel");
    let seen = receiver.seen.lock().expect("lock");
    assert!(seen.cancelled, "the device heard the cancel");
    assert!(
        !seen.uploaded.contains_key("album/big.bin"),
        "nothing whole arrived"
    );
    drop(seen);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn info_asks_the_device_who_it_is() {
    let receiver = Receiver::start(Mode::Accept);
    let (device, fingerprint) = Sender::info(&receiver.peer()).expect("info");
    assert_eq!(device.alias, "Fake Phone");
    assert_eq!(device.device_type, Some(DeviceType::Mobile));
    assert_eq!(fingerprint, None, "in the clear, no certificate");
}

#[test]
fn a_typed_address_becomes_a_peer_with_the_default_port_unless_given() {
    let p = Peer::typed("192.168.1.9");
    assert_eq!(
        (p.host.as_str(), p.port, p.protocol),
        ("192.168.1.9", PORT, Protocol::Https)
    );
    let p = Peer::typed(" 192.168.1.9:5000 ");
    assert_eq!((p.host.as_str(), p.port), ("192.168.1.9", 5000));
    let p = Peer::typed("[fe80::1]:53317");
    assert_eq!((p.host.as_str(), p.port), ("fe80::1", 53317));
    let p = Peer::typed("fe80::1");
    assert_eq!((p.host.as_str(), p.port), ("fe80::1", PORT));
    assert_eq!(p.host_header(), "[fe80::1]:53317");
    let p = Peer::typed("phone.local");
    assert_eq!((p.host.as_str(), p.port), ("phone.local", PORT));
}

#[test]
fn an_announcement_becomes_a_peer_pinned_only_over_https() {
    let device = DeviceInfo {
        alias: "Nice Orange".into(),
        version: "2.0".into(),
        device_model: Some("Samsung".into()),
        device_type: Some(DeviceType::Mobile),
        fingerprint: "abc".into(),
        port: Some(4000),
        protocol: Some(Protocol::Https),
        download: false,
    };
    let from: IpAddr = "10.0.0.7".parse().expect("ip");
    let p = Peer::from_announcement(&device, from);
    assert_eq!(
        (p.host.as_str(), p.port, p.fingerprint.as_deref()),
        ("10.0.0.7", 4000, Some("abc"))
    );
    let mut plain = device;
    plain.protocol = Some(Protocol::Http);
    plain.port = None;
    let p = Peer::from_announcement(&plain, from);
    assert_eq!(
        (p.port, p.fingerprint),
        (PORT, None),
        "a random string is not a pin"
    );
}

#[test]
fn a_device_that_is_not_there_is_an_io_error_naming_it() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    let mut peer = Peer::typed(&format!("127.0.0.1:{port}"));
    peer.protocol = Protocol::Http;
    peer.alias = "Gone".into();
    match Sender::info(&peer) {
        Err(SendError::Io(why)) => assert!(why.starts_with("Gone: "), "{why}"),
        other => panic!("{other:?}"),
    }
}
