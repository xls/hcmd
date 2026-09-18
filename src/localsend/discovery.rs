//! Finding LocalSend devices on the LAN.
//!
//! The protocol's discovery is one UDP multicast group: a device joins
//! 224.0.0.167:53317, says who it is with `announce: true`, and everybody
//! who hears answers - by `POST /register` to the announcer's port, or with
//! their own multicast message marked `announce: false`. Devices also announce
//! unprompted when their app opens. So this side does three things: keeps a
//! socket in the group and reads every message that is not its own; sends an
//! announcement when asked; and runs a small plain-HTTP responder on a port
//! of its own, because the stock app's first move on hearing us is a
//! `register` there and its second, if that fails, the multicast reply -
//! answering the first is faster and surer than waiting for the second.
//!
//! The responder answers only `register` (and `info`); a `prepare-upload`
//! sent at it is refused with a sentence, since hcmd sends and does not
//! receive.
//!
//! Multicast is not enough on its own. A device that hears us answers with
//! a `register` to our port, and a host firewall - ufw as a desktop ships
//! it - drops that on the floor, so the answer never comes or comes late by
//! multicast; a device on the same LAN may also never announce while the
//! picker is open. The stock app's remedy is a **subnet scan**: `GET /info`
//! on every host of the /24, on the protocol's port. So does this side,
//! every [`SCAN_EVERY`], on a third thread. Everything stops when the
//! [`Discovery`] is dropped, which is when the picker closes.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::protocol::{Announcement, DeviceInfo, Protocol};
use super::send::Peer;
use super::{API, MULTICAST_ADDR, PORT};
use crate::serve::http::{Response, read_request};

/// The most bytes a datagram is read into: an announcement is a few hundred.
const DATAGRAM: usize = 4096;
/// How often the listening threads look at the stop flag.
const TICK: Duration = Duration::from_millis(200);
/// The most body the responder reads: a `register` is a few hundred bytes.
const MAX_REGISTER_BODY: usize = 64 * 1024;
/// How often the subnet is scanned while the picker is open.
const SCAN_EVERY: Duration = Duration::from_secs(15);
/// How long a scan waits for one host to answer the TCP connect. A host
/// that is there answers in a millisecond; one that is not never does.
const PROBE_CONNECT: Duration = Duration::from_millis(300);
/// How many hosts are probed at once: a /24 in ~2 seconds, not minutes.
const SCAN_WORKERS: usize = 32;

/// Devices found so far, and the means of finding more.
pub struct Discovery {
    /// This side, as announced: the responder's port, `http`.
    me: DeviceInfo,
    /// What has been heard, keyed by fingerprint; newest word wins.
    found: Arc<Mutex<Vec<Peer>>>,
    /// The multicast socket, kept for announcing.
    group: UdpSocket,
    /// Which interface announcements leave by.
    interface: Ipv4Addr,
    /// Set on drop; both threads watch it.
    stop: Arc<AtomicBool>,
}

impl Discovery {
    /// Join the group, start the responder, and announce once.
    ///
    /// `interface` is the address to listen and announce on - the LAN one
    /// [`crate::serve::lan_ip`] finds - or unspecified to let the kernel
    /// choose.
    pub fn start(alias: &str, interface: Option<Ipv4Addr>) -> std::io::Result<Self> {
        let interface = interface.unwrap_or(Ipv4Addr::UNSPECIFIED);
        let stop = Arc::new(AtomicBool::new(false));
        let found = Arc::new(Mutex::new(Vec::new()));

        let responder = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        responder.set_nonblocking(true)?;
        let port = responder.local_addr()?.port();
        let me = DeviceInfo::ours(alias, port, Protocol::Http);

        let group = join_group(interface)?;
        let listener = group.try_clone()?;

        let this = Self {
            me,
            found,
            group,
            interface,
            stop,
        };
        this.spawn_listener(listener);
        this.spawn_responder(responder);
        this.spawn_scanner();
        this.announce()?;
        Ok(this)
    }

    /// This side as it introduces itself.
    #[must_use]
    pub fn me(&self) -> &DeviceInfo {
        &self.me
    }

    /// Send an announcement to the group, asking everybody to answer.
    pub fn announce(&self) -> std::io::Result<()> {
        let message = Announcement {
            device: self.me.clone(),
            announce: true,
        };
        let bytes = serde_json::to_vec(&message).map_err(std::io::Error::other)?;
        self.group
            .send_to(&bytes, SocketAddrV4::new(MULTICAST_ADDR, PORT))?;
        Ok(())
    }

    /// The devices heard from so far, by name.
    #[must_use]
    pub fn peers(&self) -> Vec<Peer> {
        let mut peers = self.found.lock().map(|f| f.clone()).unwrap_or_default();
        peers.sort_by_key(|p| p.alias.to_lowercase());
        peers
    }

    /// Fold one heard device in: what it said, and where from.
    fn heard(found: &Mutex<Vec<Peer>>, ours: &str, device: &DeviceInfo, from: IpAddr) {
        if device.fingerprint == ours || device.alias.is_empty() {
            return;
        }
        let peer = Peer::from_announcement(device, from);
        if let Ok(mut list) = found.lock() {
            match list.iter_mut().find(|p| {
                p.fingerprint.as_deref() == Some(device.fingerprint.as_str())
                    || (p.host == peer.host && p.port == peer.port)
            }) {
                Some(known) => *known = peer,
                None => list.push(peer),
            }
        }
    }

    /// What one multicast datagram says, if it is an announcement from
    /// somebody else: a peer, and whether they asked to be answered.
    #[must_use]
    pub fn parse_datagram(bytes: &[u8]) -> Option<Announcement> {
        serde_json::from_slice(bytes).ok()
    }

    fn spawn_listener(&self, socket: UdpSocket) {
        let found = Arc::clone(&self.found);
        let stop = Arc::clone(&self.stop);
        let ours = self.me.fingerprint.clone();
        std::thread::Builder::new()
            .name("localsend-listen".into())
            .spawn(move || {
                let mut buf = [0_u8; DATAGRAM];
                while !stop.load(Ordering::Relaxed) {
                    let Ok((n, from)) = socket.recv_from(&mut buf) else {
                        continue;
                    };
                    let Some(message) = Self::parse_datagram(buf.get(..n).unwrap_or(&[])) else {
                        continue;
                    };
                    Self::heard(&found, &ours, &message.device, from.ip());
                }
            })
            .ok();
    }

    fn spawn_responder(&self, listener: TcpListener) {
        let found = Arc::clone(&self.found);
        let stop = Arc::clone(&self.stop);
        let me = self.me.clone();
        std::thread::Builder::new()
            .name("localsend-register".into())
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let Ok((mut stream, from)) = listener.accept() else {
                        std::thread::sleep(TICK);
                        continue;
                    };
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                    let response = match read_request(&mut stream, MAX_REGISTER_BODY) {
                        Ok((request, body)) => respond(&me, &found, &request, &body, from),
                        Err(_) => Response::text(400, "bad request"),
                    };
                    let _ = write(&mut stream, &response);
                }
            })
            .ok();
    }
}

impl Discovery {
    /// The subnet scan, when the interface is a real LAN address; nothing
    /// is scanned from an unspecified or loopback one.
    fn spawn_scanner(&self) {
        let interface = self.interface;
        if interface.is_unspecified() || interface.is_loopback() {
            return;
        }
        let found = Arc::clone(&self.found);
        let stop = Arc::clone(&self.stop);
        let ours = self.me.fingerprint.clone();
        std::thread::Builder::new()
            .name("localsend-scan".into())
            .spawn(move || {
                loop {
                    scan_subnet(interface, &stop, |device, ip| {
                        Self::heard(&found, &ours, &device, IpAddr::V4(ip));
                    });
                    let mut waited = Duration::ZERO;
                    while waited < SCAN_EVERY {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(TICK);
                        waited = waited.saturating_add(TICK);
                    }
                }
            })
            .ok();
    }
}

/// Every host of `interface`'s /24 but the interface itself, `.1` to `.254`.
///
/// A /24 by assumption: [`crate::serve::lan_ip`] learns the address by
/// connecting a UDP socket and not the prefix length, and a /24 is what
/// nearly every home and office LAN is. A wider network (the owner's is a
/// /23) has its other half reached by multicast or a typed address.
#[must_use]
pub fn subnet_hosts(interface: Ipv4Addr) -> Vec<Ipv4Addr> {
    let [a, b, c, own] = interface.octets();
    (1..=254_u8)
        .filter(|d| *d != own)
        .map(|d| Ipv4Addr::new(a, b, c, d))
        .collect()
}

/// Probe every host of the /24, [`SCAN_WORKERS`] at a time, and hand each
/// device found to `heard`. Stops early when `stop` is set.
fn scan_subnet(
    interface: Ipv4Addr,
    stop: &AtomicBool,
    heard: impl Fn(DeviceInfo, Ipv4Addr) + Sync,
) {
    let hosts = subnet_hosts(interface);
    let chunk = hosts.len().div_ceil(SCAN_WORKERS).max(1);
    std::thread::scope(|scope| {
        for part in hosts.chunks(chunk) {
            let heard = &heard;
            scope.spawn(move || {
                for ip in part {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    if let Some((device, protocol)) = probe_host(IpAddr::V4(*ip), PORT) {
                        let mut device = device;
                        device.port = Some(PORT);
                        device.protocol = Some(protocol);
                        heard(device, *ip);
                    }
                }
            });
        }
    });
}

/// Ask the host at `ip:port` who it is: a quick TCP connect to see whether
/// anything listens, then `GET /info` in the clear and, when that gets no
/// answer, over TLS.
///
/// The clear first, on purpose. A TLS device meets plaintext with a closed
/// socket at once, so the fallback costs one round trip; the other order
/// deadlocks against a plain device - a ClientHello has no blank line, and
/// an HTTP server that reads until one waits as long as the client does.
///
/// Over TLS the certificate presented must hash to the fingerprint the
/// `info` claims (compared without regard to case: devices write it in
/// upper-case hex, the hash in lower); a host whose two identities disagree
/// is not a device to send to, and is skipped.
#[must_use]
pub fn probe_host(ip: IpAddr, port: u16) -> Option<(DeviceInfo, Protocol)> {
    let listening = TcpStream::connect_timeout(&SocketAddr::new(ip, port), PROBE_CONNECT).is_ok();
    if !listening {
        return None;
    }
    let host = ip.to_string();
    if let Ok(mut connection) = super::tls::connect_plain(&host, port)
        && let Some(device) = info_over(connection.stream.as_mut(), &host, port)
    {
        return Some((device, Protocol::Http));
    }
    let mut connection = super::tls::connect_tls(&host, port, None).ok()?;
    let device = info_over(connection.stream.as_mut(), &host, port)?;
    let seen = connection.fingerprint.as_deref().unwrap_or("");
    if !seen.eq_ignore_ascii_case(&device.fingerprint) {
        return None;
    }
    Some((device, Protocol::Https))
}

/// `GET /info` on an open connection, as a device.
fn info_over(stream: &mut dyn super::tls::Stream, host: &str, port: u16) -> Option<DeviceInfo> {
    let host_header = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let response = super::http::request(
        stream,
        &host_header,
        "GET",
        &format!("{API}/info"),
        super::http::Body::None,
        &mut |_| true,
    )
    .ok()?;
    if response.status != 200 {
        return None;
    }
    serde_json::from_slice(&response.body).ok()
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self
            .group
            .leave_multicast_v4(&MULTICAST_ADDR, &self.interface);
    }
}

/// The responder's answer to one request.
fn respond(
    me: &DeviceInfo,
    found: &Mutex<Vec<Peer>>,
    request: &crate::serve::http::Request,
    body: &[u8],
    from: SocketAddr,
) -> Response {
    let register = format!("{API}/register");
    let info = format!("{API}/info");
    let prepare = format!("{API}/prepare-upload");
    match (request.method.as_str(), request.path.as_str()) {
        ("POST", path) if path == register => {
            if let Ok(device) = serde_json::from_slice::<DeviceInfo>(body) {
                Discovery::heard(found, &me.fingerprint, &device, from.ip());
            }
            answer_with(me)
        }
        ("GET", path) if path == info => answer_with(me),
        ("POST", path) if path == prepare => {
            Response::text(403, "this hcmd only sends; it does not receive files")
        }
        _ => Response::text(404, "not here"),
    }
}

/// Our device as a `register` or `info` answer: no port, no protocol, as
/// the specification writes the answer.
fn answer_with(me: &DeviceInfo) -> Response {
    let mut answer = me.clone();
    answer.port = None;
    answer.protocol = None;
    match serde_json::to_vec(&answer) {
        Ok(json) => Response::new(200).bytes("application/json", json),
        Err(_) => Response::text(500, "could not describe this device"),
    }
}

/// Write a response with a byte body.
fn write(stream: &mut std::net::TcpStream, response: &Response) -> std::io::Result<()> {
    use std::io::Write;
    stream.write_all(&response.head())?;
    if let crate::serve::http::Body::Bytes(bytes) = &response.body {
        stream.write_all(bytes)?;
    }
    stream.flush()?;
    let _ = stream.shutdown(std::net::Shutdown::Both);
    Ok(())
}

/// A UDP socket in the multicast group on `interface`, with the port shared:
/// the stock app on this same machine is already bound to it, and without
/// address reuse a second bind fails.
fn join_group(interface: Ipv4Addr) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol as SockProtocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(SockProtocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.set_read_timeout(Some(TICK))?;
    socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT).into())?;
    socket.join_multicast_v4(&MULTICAST_ADDR, &interface)?;
    socket.set_multicast_if_v4(&interface)?;
    socket.set_multicast_loop_v4(true)?;
    Ok(socket.into())
}

#[cfg(test)]
#[path = "discovery_tests.rs"]
mod tests;
