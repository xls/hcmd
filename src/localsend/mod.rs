//! Sending files to a LocalSend device on the LAN.
//!
//! [LocalSend](https://localsend.org) is an open protocol - one multicast
//! announcement to find each other, then a few JSON requests over HTTPS to
//! hand files across - spoken by a stock app on every phone and desktop. hcmd
//! speaks the sending half: pick a device, the transfer runs as a job, the
//! other side sees its usual accept prompt. It is written here on
//! `std::net`, `rustls` and `serde` rather than pulled in as a library,
//! because the libraries that exist bring a second HTTP stack for a protocol
//! that is four endpoints.
//!
//! The pieces: [`protocol`] is the wire format and nothing else, tested
//! against the specification's own examples; [`tls`] connects to a device
//! and pins the fingerprint it announced; [`http`] writes a request and reads
//! an answer on that connection; [`send`] talks to one device - prepare,
//! upload each file, cancel - and reports progress; [`discovery`] finds
//! devices by multicast. Nothing here receives; a receiving side would need
//! a TLS server and is a separate piece of work.

pub mod discovery;
#[cfg(test)]
pub mod fake;
pub mod http;
pub mod protocol;
pub mod send;
pub mod tls;

pub use discovery::Discovery;
pub use protocol::{DeviceInfo, DeviceType, FileMeta, PROTOCOL_VERSION, Protocol};
pub use send::{Peer, SendError, Sender, Session, collect};

/// The multicast group the protocol announces on.
pub const MULTICAST_ADDR: std::net::Ipv4Addr = std::net::Ipv4Addr::new(224, 0, 0, 167);
/// The multicast port, and the default TCP port a device listens on.
pub const PORT: u16 = 53317;
/// The path every endpoint hangs under.
pub const API: &str = "/api/localsend/v2";
