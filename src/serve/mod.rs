//! `Ctrl+N`: serve the selection over HTTP and WebDAV, for as long as the
//! dialog is open.
//!
//! The mirror of [`crate::net::download`]: where that fetches a file the user
//! pointed at, this hands out the files the user selected, to a browser on
//! the same network or to another hcmd connecting with `dav://`. It is a
//! small, read-only static server over `std::net` - `GET`, `HEAD`, `OPTIONS`
//! and `PROPFIND`, with `Range` so downloads from it resume - and it lives
//! exactly as long as its dialog: closing the dialog stops the listener, and
//! nothing serves in the background of a program that says it does not.
//!
//! Three pieces, each a pure function of its input so the wire format is
//! tested without a socket: [`http`] parses a request and builds a response,
//! [`tree`] maps a URL path onto the selected files and refuses one that could
//! leave them, and [`dav`] writes the `PROPFIND` answer a WebDAV client lists
//! the share with.

pub mod dav;
pub mod firewall;
pub mod http;
pub mod server;
pub mod tree;

pub use server::{SERVE_CHANNEL_DEPTH, ServeEvent, Served, Server, lan_ip};
