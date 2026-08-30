//! `holoscommander` - a Total Commander alternative for the terminal, with the
//! default keys mapped identical to Total Commander (binary: `hcmd`).
//!
//! Two panels, the classic function keys, and one abstraction underneath
//! everything: [`vfs::Vfs`]. A local directory, a member of a zip, a file on an
//! SFTP host and a file on a partition of a disk image are the same thing to
//! every layer above it, which is why copying, viewing and searching do not
//! each need to know where a file lives.
//!
//! The crate is a library plus a thin binary, so that the input state machine
//! and the event loop can be exercised from integration tests without a
//! terminal. That is what makes it possible to drive the whole program in a
//! test: [`input::dispatch`] turns a key into an intention and performs no I/O
//! at all, and [`runtime`] is the only place that blocks.
//!
//! `AGENTS.md` in the repository root is the map for a first change: what each
//! module owns, the invariants that are load-bearing rather than stylistic, and
//! the traps that have each cost real time.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod app;
pub mod config;
pub mod console;
pub mod devices;
pub mod dialog;
pub mod error;
pub mod input;
pub mod net;
pub mod ops;
pub mod panel;
pub mod remote;
pub mod rename;
pub mod runtime;
pub mod search;
pub mod term;
pub mod ui;
pub mod vfs;
pub mod viewer;

pub use error::{Error, Result};

/// The package version, for `--version` and the `F1` About page.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The binary's name (package `holoscommander`, binary `hcmd`).
pub const BIN_NAME: &str = "hcmd";
