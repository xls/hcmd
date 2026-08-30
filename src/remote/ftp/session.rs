//! What this server can do, and the one trait that hides which stream it is
//! spoken over.
//!
//! An FTP server is asked once what it supports and the answer is kept for the
//! connection: whether `MLSD` is there, whether `SIZE` is, whether `REST` is.
//! Every later decision reads those facts rather than trying a command and
//! catching the failure, so a server that lacks a feature costs one round trip
//! at connect and none afterwards.
//!
//! The trait exists because a control connection may be plain or wrapped in
//! TLS and nothing above this cares which. It is the only place that knows.

use super::parse::unix_time;
use super::*;

/// One control connection, with the TLS type erased.
///
/// suppaftp's `FtpStream` and `RustlsFtpStream` are two types (`ImplFtpStream`
/// over two different stream types), so a plain and a TLS connection cannot
/// sit in one pool without this. Every method is one FTP command, and every
/// one of them blocks.
pub(super) trait Session: Send {
    /// USER then PASS. The one place a password is passed to suppaftp
    /// (the design, item 4).
    fn login(&mut self, user: &str, password: &str) -> FtpResult<()>;
    /// `TYPE I`.
    fn binary(&mut self) -> FtpResult<()>;
    /// Set, or with `None` drop, the read timeout on the control socket.
    /// The greeting is bounded by one; a transfer is not, and a farewell
    /// `QUIT` is bounded again so it cannot wait for ever
    /// ([`Pool::close`]).
    fn set_read_timeout(&mut self, timeout: Option<Duration>);
    /// `PWD`.
    fn pwd(&mut self) -> FtpResult<String>;
    /// `FEAT`.
    fn feat(&mut self) -> FtpResult<Features>;
    /// `MLSD`.
    fn mlsd(&mut self, dir: &str) -> FtpResult<Vec<String>>;
    /// `LIST`.
    fn list(&mut self, dir: &str) -> FtpResult<Vec<String>>;
    /// `MLST`, one line of facts.
    fn mlst(&mut self, path: &str) -> FtpResult<String>;
    /// `SIZE`.
    fn size(&mut self, path: &str) -> FtpResult<u64>;
    /// `MDTM`, which RFC 3659 defines in UTC.
    fn mdtm(&mut self, path: &str) -> FtpResult<Option<SystemTime>>;
    /// `MKD`.
    fn mkdir(&mut self, path: &str) -> FtpResult<()>;
    /// `DELE`.
    fn rm(&mut self, path: &str) -> FtpResult<()>;
    /// `RMD`.
    fn rmdir(&mut self, path: &str) -> FtpResult<()>;
    /// `RNFR` then `RNTO`.
    fn rename(&mut self, from: &str, to: &str) -> FtpResult<()>;
    /// `RETR`, returning the data connection.
    fn retr_start(&mut self, path: &str) -> FtpResult<Box<dyn Read + Send>>;
    /// Close the data connection and read the transfer's result.
    fn retr_finish(&mut self, data: Box<dyn Read + Send>) -> FtpResult<()>;
    /// `ABOR`: the reader was dropped before the end of the file.
    fn retr_abort(&mut self, data: Box<dyn Read + Send>) -> FtpResult<()>;
    /// `STOR`, returning the data connection.
    fn stor_start(&mut self, path: &str) -> FtpResult<Box<dyn Write + Send>>;
    /// Close the data connection and read the transfer's result. This is the
    /// commit.
    fn stor_finish(&mut self, data: Box<dyn Write + Send>) -> FtpResult<()>;
    /// `QUIT`.
    fn quit(&mut self) -> FtpResult<()>;
}

impl<T: TlsStream + Send + 'static> Session for ImplFtpStream<T> {
    fn login(&mut self, user: &str, password: &str) -> FtpResult<()> {
        ImplFtpStream::login(self, user, password)
    }

    fn binary(&mut self) -> FtpResult<()> {
        self.transfer_type(FileType::Binary)
    }

    fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        let _ = self.get_ref().set_read_timeout(timeout);
    }

    fn pwd(&mut self) -> FtpResult<String> {
        ImplFtpStream::pwd(self)
    }

    fn feat(&mut self) -> FtpResult<Features> {
        ImplFtpStream::feat(self)
    }

    fn mlsd(&mut self, dir: &str) -> FtpResult<Vec<String>> {
        ImplFtpStream::mlsd(self, Some(dir))
    }

    fn list(&mut self, dir: &str) -> FtpResult<Vec<String>> {
        ImplFtpStream::list(self, Some(dir))
    }

    fn mlst(&mut self, path: &str) -> FtpResult<String> {
        ImplFtpStream::mlst(self, Some(path))
    }

    fn size(&mut self, path: &str) -> FtpResult<u64> {
        ImplFtpStream::size(self, path).map(|size| u64::try_from(size).unwrap_or(u64::MAX))
    }

    fn mdtm(&mut self, path: &str) -> FtpResult<Option<SystemTime>> {
        // RFC 3659 defines MDTM's answer in UTC, so this one is exact rather
        // than the guess a LIST line forces (see [`parse_list`]).
        ImplFtpStream::mdtm(self, path).map(|naive| unix_time(naive.and_utc().timestamp()))
    }

    fn mkdir(&mut self, path: &str) -> FtpResult<()> {
        ImplFtpStream::mkdir(self, path)
    }

    fn rm(&mut self, path: &str) -> FtpResult<()> {
        ImplFtpStream::rm(self, path)
    }

    fn rmdir(&mut self, path: &str) -> FtpResult<()> {
        ImplFtpStream::rmdir(self, path)
    }

    fn rename(&mut self, from: &str, to: &str) -> FtpResult<()> {
        ImplFtpStream::rename(self, from, to)
    }

    fn retr_start(&mut self, path: &str) -> FtpResult<Box<dyn Read + Send>> {
        let stream = self.retr_as_stream(path)?;
        Ok(Box::new(stream))
    }

    fn retr_finish(&mut self, data: Box<dyn Read + Send>) -> FtpResult<()> {
        self.finalize_retr_stream(data)
    }

    fn retr_abort(&mut self, data: Box<dyn Read + Send>) -> FtpResult<()> {
        self.abort(data)
    }

    fn stor_start(&mut self, path: &str) -> FtpResult<Box<dyn Write + Send>> {
        let stream = self.put_with_stream(path)?;
        Ok(Box::new(stream))
    }

    fn stor_finish(&mut self, data: Box<dyn Write + Send>) -> FtpResult<()> {
        self.finalize_put_stream(data)
    }

    fn quit(&mut self) -> FtpResult<()> {
        ImplFtpStream::quit(self)
    }
}

/// What `FEAT` said this server can do (RFC 2389).
///
/// Atomics rather than a lock, because they are read on every listing and
/// written only when a server contradicts its own `FEAT` - and because
/// the house rules forbid `Cell`.
pub(super) struct ServerFacts {
    pub(super) mlsd: AtomicBool,
    pub(super) mlst: AtomicBool,
    pub(super) size: AtomicBool,
    pub(super) mdtm: AtomicBool,
}

impl ServerFacts {
    /// Ask the server. A server that will not answer `FEAT` at all is assumed
    /// to have none of these, which lands on `LIST` and the parent listing -
    /// the paths that work everywhere.
    pub(super) fn probe(session: &mut dyn Session) -> Self {
        let features = session.feat().unwrap_or_default();
        Self {
            mlsd: AtomicBool::new(has(&features, "MLSD")),
            mlst: AtomicBool::new(has(&features, "MLST")),
            size: AtomicBool::new(has(&features, "SIZE")),
            mdtm: AtomicBool::new(has(&features, "MDTM")),
        }
    }
}

/// Whether `FEAT` listed a command, whatever case the server chose.
pub(super) fn has(features: &Features, name: &str) -> bool {
    features.keys().any(|key| key.eq_ignore_ascii_case(name))
}
