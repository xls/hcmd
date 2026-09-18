//! Hearing devices: the responder over a real socket, the datagram parser
//! over the specification's message, and the group itself when the machine
//! allows.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};

use super::*;
use crate::localsend::protocol::DeviceType;

/// A device as the stock app would `register` itself.
fn phone() -> DeviceInfo {
    DeviceInfo {
        alias: "Nice Orange".into(),
        version: "2.1".into(),
        device_model: Some("Samsung".into()),
        device_type: Some(DeviceType::Mobile),
        fingerprint: "orange-fp".into(),
        port: Some(53317),
        protocol: Some(Protocol::Https),
        download: false,
    }
}

/// One raw HTTP exchange with the responder.
fn exchange(port: u16, request: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream.write_all(request.as_bytes()).expect("write");
    let mut out = String::new();
    stream.read_to_string(&mut out).expect("read");
    out
}

/// Wait, briefly, for the listening thread to fold a message in, and give
/// back only the rows named `alias`. Every test here joins the real group on
/// this machine, so a discovery hears the other tests' announcements - and
/// any stock app that is running - and the count of everything means
/// nothing; the rows for one name do.
fn settle(discovery: &Discovery, alias: &str) -> Vec<Peer> {
    let named = |peers: Vec<Peer>| -> Vec<Peer> {
        peers.into_iter().filter(|p| p.alias == alias).collect()
    };
    for _ in 0..50 {
        let peers = named(discovery.peers());
        if !peers.is_empty() {
            return peers;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    named(discovery.peers())
}

#[test]
fn the_specifications_announcement_parses_and_an_unrelated_datagram_does_not() {
    let message = Discovery::parse_datagram(
        br#"{"alias":"Nice Orange","version":"2.0","deviceModel":"Samsung","deviceType":"mobile","fingerprint":"x","port":53317,"protocol":"https","download":true,"announce":true}"#,
    )
    .expect("an announcement");
    assert_eq!(message.device.alias, "Nice Orange");
    assert!(message.announce);
    assert!(Discovery::parse_datagram(b"M-SEARCH * HTTP/1.1").is_none());
    assert!(
        Discovery::parse_datagram(b"{\"hello\":1}").is_none(),
        "not a device"
    );
}

#[test]
fn a_device_that_registers_with_the_responder_is_listed_and_answered_with_who_we_are() {
    let discovery = Discovery::start("hcmd test", Some(Ipv4Addr::LOCALHOST)).expect("start");
    let port = discovery.me().port.expect("a responder port");
    let body = serde_json::to_string(&phone()).expect("json");
    let answer = exchange(
        port,
        &format!(
            "POST /api/localsend/v2/register HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    );
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    let json = answer.split("\r\n\r\n").nth(1).expect("a body");
    let us: DeviceInfo = serde_json::from_str(json).expect("our device");
    assert_eq!(us.alias, "hcmd test");
    assert_eq!(us.fingerprint, discovery.me().fingerprint);
    assert_eq!(us.port, None, "an answer carries no port");

    let peers = settle(&discovery, "Nice Orange");
    assert_eq!(peers.len(), 1, "{peers:?}");
    assert_eq!(peers[0].alias, "Nice Orange");
    assert_eq!(peers[0].host, "127.0.0.1", "where the register came from");
    assert_eq!(peers[0].port, 53317);
    assert_eq!(peers[0].fingerprint.as_deref(), Some("orange-fp"));

    // The same device again, from the same place, is still one row.
    exchange(
        port,
        &format!(
            "POST /api/localsend/v2/register HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    );
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(settle(&discovery, "Nice Orange").len(), 1);
}

#[test]
fn the_responder_refuses_to_receive_and_answers_info() {
    let discovery = Discovery::start("hcmd test", Some(Ipv4Addr::LOCALHOST)).expect("start");
    let port = discovery.me().port.expect("port");
    let refused = exchange(
        port,
        "POST /api/localsend/v2/prepare-upload HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\n{}",
    );
    assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
    assert!(refused.contains("only sends"), "{refused}");
    let info = exchange(
        port,
        "GET /api/localsend/v2/info HTTP/1.1\r\nHost: x\r\n\r\n",
    );
    assert!(info.starts_with("HTTP/1.1 200"), "{info}");
    assert!(info.contains("\"alias\":\"hcmd test\""), "{info}");
    let lost = exchange(port, "GET /nothing HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(lost.starts_with("HTTP/1.1 404"), "{lost}");
}

#[test]
fn our_own_announcement_is_not_a_peer_and_dropping_stops_the_threads() {
    let discovery = Discovery::start("hcmd self-test", Some(Ipv4Addr::LOCALHOST)).expect("start");
    // `start` announced once already; announce again and make sure we never
    // list ourselves, whether or not the multicast loops back here. Other
    // tests' devices may well be heard; ours must not be.
    discovery.announce().expect("announce");
    std::thread::sleep(std::time::Duration::from_millis(100));
    let ours = discovery.me().fingerprint.clone();
    assert!(
        discovery
            .peers()
            .iter()
            .all(|p| p.alias != "hcmd self-test" && p.fingerprint.as_deref() != Some(ours.as_str())),
        "{:?}",
        discovery.peers()
    );
    let port = discovery.me().port.expect("port");
    drop(discovery);
    std::thread::sleep(TICK.saturating_mul(2));
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_err(),
        "the responder is gone with the discovery"
    );
}

#[test]
fn the_subnet_is_every_host_of_the_slash_24_but_this_one() {
    let hosts = subnet_hosts(Ipv4Addr::new(192, 168, 10, 103));
    assert_eq!(hosts.len(), 253);
    assert_eq!(hosts.first(), Some(&Ipv4Addr::new(192, 168, 10, 1)));
    assert_eq!(hosts.last(), Some(&Ipv4Addr::new(192, 168, 10, 254)));
    assert!(!hosts.contains(&Ipv4Addr::new(192, 168, 10, 103)));
    assert!(!hosts.contains(&Ipv4Addr::new(192, 168, 10, 0)));
    assert!(!hosts.contains(&Ipv4Addr::new(192, 168, 10, 255)));
}

#[test]
fn a_probe_finds_a_device_in_the_clear_and_says_so() {
    let receiver = crate::localsend::fake::Receiver::start(crate::localsend::fake::Mode::Accept);
    let (device, protocol) =
        probe_host(IpAddr::V4(Ipv4Addr::LOCALHOST), receiver.port).expect("a device");
    assert_eq!(device.alias, "Fake Phone");
    assert_eq!(protocol, Protocol::Http, "the clear worked");
}

#[test]
fn a_probe_of_a_closed_port_is_none_and_quick() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    let start = std::time::Instant::now();
    assert!(probe_host(IpAddr::V4(Ipv4Addr::LOCALHOST), port).is_none());
    assert!(
        start.elapsed() < std::time::Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
}

/// A second member of the group on this machine hears the first's
/// announcement. Needs multicast on the loopback interface, which a
/// container or a hardened kernel may not have; so it is ignored by default
/// and run by hand: `cargo test --lib -- --ignored localsend::discovery`.
#[test]
#[ignore = "needs multicast on loopback"]
fn two_members_of_the_group_on_one_machine_hear_each_other() {
    let a = Discovery::start("hcmd a", Some(Ipv4Addr::LOCALHOST)).expect("a");
    let b = Discovery::start("hcmd b", Some(Ipv4Addr::LOCALHOST)).expect("b");
    b.announce().expect("announce");
    a.announce().expect("announce");
    assert_eq!(settle(&a, "hcmd b").len(), 1);
    assert_eq!(settle(&b, "hcmd a").len(), 1);
}

/// Not a test: a probe. Joins the group on the LAN interface, announces, and
/// prints who answers within three seconds. Run by hand when a device is
/// expected to be there:
/// `cargo test --lib -- --ignored --nocapture what_is_on_the_lan_right_now`.
#[test]
#[ignore = "a manual probe of the real LAN"]
fn what_is_on_the_lan_right_now() {
    let lan = crate::serve::lan_ip().and_then(|ip| match ip {
        std::net::IpAddr::V4(v4) => Some(v4),
        std::net::IpAddr::V6(_) => None,
    });
    if let Some(lan) = lan {
        let start = std::time::Instant::now();
        let hosts = subnet_hosts(lan).len();
        let discovery = Discovery::start("hcmd probe", Some(lan)).expect("start");
        std::thread::sleep(std::time::Duration::from_secs(4));
        eprintln!(
            "scan of {hosts} hosts plus 4s wait took {:?}; heard {}",
            start.elapsed(),
            discovery.peers().len()
        );
    }
    let discovery = Discovery::start("hcmd probe", lan).expect("start");
    eprintln!("announcing on {lan:?} as {}", discovery.me().alias);
    for _ in 0..3 {
        std::thread::sleep(std::time::Duration::from_secs(1));
        discovery.announce().expect("announce");
    }
    for peer in discovery.peers() {
        eprintln!(
            "heard: {:<24} {:?} {:?} {}:{} {:?} fp={:?}",
            peer.alias,
            peer.device_type,
            peer.model,
            peer.host,
            peer.port,
            peer.protocol,
            peer.fingerprint
        );
    }
    eprintln!("{} device(s)", discovery.peers().len());
}

/// Not a test: `probe_host` on one real address, by hand:
/// `HCMD_LS_PROBE_HOST=192.168.10.10 cargo test --lib -- --ignored --nocapture probe_one_host`.
#[test]
#[ignore = "a manual probe of one real device"]
fn probe_one_host() {
    let Ok(host) = std::env::var("HCMD_LS_PROBE_HOST") else {
        return;
    };
    let ip: IpAddr = host.parse().expect("an ip");
    let start = std::time::Instant::now();
    let found = probe_host(ip, PORT);
    eprintln!("probe of {ip} took {:?}: {found:?}", start.elapsed());
}
