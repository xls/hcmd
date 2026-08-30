//! the quick-connect line.
//!
//! > Typing `thorin@nas.local` or a full `sftp://user@host:port/path` connects
//! > directly. A bare `host` and a `user@host` both work.
//!
//! One rule dominates the rest and is why this module exists at all: a
//! password typed on the line - the own first example is
//! `thorin:pass@192.168.1.10` - is used for that connection and **nothing
//! else**. It never reaches `hosts.toml`, a history, a header, a status
//! line, an error, or a `Debug`. [`Parsed`] can derive `Debug` only because
//! [`Secret`]'s own `Debug` redacts, and [`redact`] is the single function
//! that turns a typed line into the form anything else is allowed to
//! remember.
//!
//! This module touches neither the filesystem nor the network.

use crate::remote::secret::Secret;
use crate::remote::{Protocol, Target};

/// What the quick-connect line parsed to.
///
/// `Debug` is derived and that is safe **only** because [`Secret`]'s own
/// `Debug` redacts (the design, S3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    /// Where it points. Secret-free, as every [`Target`] is.
    pub target: Target,
    /// A password typed on the line. Session-only, never stored.
    pub password: Option<Secret>,
}

/// Why a connect line was refused, phrased for the dialog's error row.
///
/// No variant quotes the line back, because the line is the one place in this
/// program where a credential is typed in the clear (S3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Nothing was typed.
    Empty,
    /// A `scheme://` this program does not speak.
    UnknownScheme(String),
    /// The port was not a number, or was zero.
    BadPort(String),
    /// There was a user, or a scheme, but no host.
    NoHost,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("type a host to connect to"),
            Self::UnknownScheme(scheme) => write!(
                f,
                "{scheme}: not a protocol this program speaks; \
                 use sftp, ftp, ftps or ftps-implicit"
            ),
            Self::BadPort(port) => write!(f, "{port}: not a port number"),
            Self::NoHost => f.write_str("that line names no host"),
        }
    }
}

/// The user an FTP line with no user in it logs in as (the design's
/// `ftp://anonymous@ftp.example.org`, and the rule that it is never
/// prompted).
const ANONYMOUS: &str = "anonymous";

/// Split off a `scheme://` prefix, if there is one.
///
/// `Err` for a scheme this program does not speak, so a typo is refused rather
/// than silently treated as a hostname called `htp`.
fn split_scheme(line: &str) -> std::result::Result<(Option<Protocol>, &str), ParseError> {
    match line.split_once("://") {
        None => Ok((None, line)),
        Some((scheme, rest)) => match Protocol::parse(scheme) {
            Some(protocol) => Ok((Some(protocol), rest)),
            None => Err(ParseError::UnknownScheme(scheme.to_string())),
        },
    }
}

/// Split `user[:password]@host...` at the last `@` **of the authority**.
///
/// The last, not the first: a password is allowed to contain one, and a
/// hostname is not.
///
/// Of the authority, not of the line: `@` is an ordinary character in a
/// directory name, and the own examples put an initial directory on the line.
/// Searching the whole remainder made
/// `sftp://thorin:pass@nas.local/srv/@scope` split at the `@` in the *path* -
/// host `scope`, password `pass@nas.local/srv/` - and dial a host the user
/// never named with the password they typed. The authority ends at the first
/// `/`, which is where [`split_host`] already ends it; the two functions have
/// to agree about that or the line means two different things depending on
/// which one is asked (RFC 3986 §3.2 splits the userinfo inside the authority
/// for the same reason).
fn split_userinfo(rest: &str) -> (Option<&str>, &str) {
    let authority = rest.find('/').unwrap_or(rest.len());
    match rest.get(..authority).and_then(|auth| auth.rfind('@')) {
        Some(at) => (rest.get(..at), rest.get(at + 1..).unwrap_or("")),
        None => (None, rest),
    }
}

/// Split `host[:port][/dir]` into its three parts.
///
/// A bracketed literal `[::1]` is an IPv6 host, so the colons inside it are
/// not a port rule 5). Everything after the host and a colon, up to the first
/// `/`, is the port - which is what the design means by "everything after the
/// host up to the first `/`".
fn split_host(rest: &str) -> (&str, Option<&str>, Option<&str>) {
    let (authority, dir) = match rest.find('/') {
        Some(slash) => (
            rest.get(..slash).unwrap_or(""),
            rest.get(slash..).filter(|d| !d.is_empty()),
        ),
        None => (rest, None),
    };
    // Search for the port colon after the closing bracket of an IPv6 literal,
    // and anywhere for a name or an IPv4 address.
    let from = match authority.rfind(']') {
        Some(close) => close.saturating_add(1),
        None => 0,
    };
    let colon = authority.get(from..).and_then(|tail| {
        tail.find(':')
            .map(|at| at.saturating_add(from))
            .filter(|_| authority.get(from..).is_some_and(|t| !t.contains("::")))
    });
    match colon {
        Some(at) => (
            authority.get(..at).unwrap_or(""),
            authority.get(at + 1..),
            dir,
        ),
        None => (authority, None, dir),
    }
}

/// Parse `thorin:pass@192.168.1.10`, `sftp://thorin@nas.local:2222/srv/media`,
/// `ftp://anonymous@ftp.example.org`, `thorin@buildbox`, `buildbox`.
///
///
/// Rules, in the order they are applied:
/// 1. A `scheme://` prefix sets the protocol; absent, it is `default`, which
///    comes from `[remote] default_protocol` and ships as `sftp`.
/// 2. Everything before the last `@` is `user[:password]`; absent, the user is
///    `user` for the SSH family and `anonymous` for FTP.
/// 3. After the host, `:` up to the first `/` is the port; absent, it is
///    [`Protocol::default_port`].
/// 4. The rest is the initial directory; absent, `None`.
/// 5. A bracketed literal `[::1]` is an IPv6 host, so the colons inside it are
///    not a port.
pub fn parse(line: &str, default: Protocol, user: &str) -> std::result::Result<Parsed, ParseError> {
    let line = line.trim();
    if line.is_empty() {
        return Err(ParseError::Empty);
    }
    let (scheme, rest) = split_scheme(line)?;
    let protocol = scheme.unwrap_or(default);
    let (userinfo, rest) = split_userinfo(rest);
    let (host, port, dir) = split_host(rest);
    if host.is_empty() {
        return Err(ParseError::NoHost);
    }
    let port = match port {
        None => protocol.default_port(),
        Some(text) => match text.parse::<u16>() {
            Ok(0) | Err(_) => return Err(ParseError::BadPort(text.to_string())),
            Ok(port) => port,
        },
    };
    let (name, password) = match userinfo {
        None => (None, None),
        Some(info) => match info.split_once(':') {
            None => (Some(info), None),
            Some((name, secret)) => (Some(name), Some(Secret::from_str(secret))),
        },
    };
    let name = match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => name.to_string(),
        // the `ftp://anonymous@ftp.example.org` is what an FTP line with
        // no user has to mean; anything else would prompt for a password
        // nobody has.
        None if protocol.verifies_host_key() => user.to_string(),
        None => ANONYMOUS.to_string(),
    };
    Ok(Parsed {
        target: Target {
            protocol,
            host: host.to_string(),
            port,
            user: name,
            dir: dir.map(str::to_string),
        },
        password,
    })
}

/// The line as it is safe to remember: the same string with any password
/// removed.
///
/// This is the only form of a typed connect line that is ever written
/// anywhere - a history, a `Debug`, a log line, an error. It is deliberately
/// textual rather than a re-rendering of [`Parsed`], so a line that did not
/// parse is still redactable: the moment a password is on screen is exactly
/// the moment the line is most likely to be malformed.
pub fn redact(line: &str) -> String {
    let (head, rest) = match line.split_once("://") {
        Some((scheme, rest)) => (format!("{scheme}://"), rest),
        None => (String::new(), line),
    };
    // The authority's last `@`, exactly as [`split_userinfo`] finds it: a `@`
    // in the initial directory belongs to the path, and cutting there mangles
    // the line this function is contracted to leave otherwise untouched.
    let authority = rest.find('/').unwrap_or(rest.len());
    let Some(at) = rest.get(..authority).and_then(|auth| auth.rfind('@')) else {
        return line.to_string();
    };
    let userinfo = rest.get(..at).unwrap_or("");
    let tail = rest.get(at..).unwrap_or("");
    match userinfo.split_once(':') {
        None => line.to_string(),
        Some((name, _)) => format!("{head}{name}{tail}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sftp(line: &str) -> Parsed {
        parse(line, Protocol::Sftp, "thorin").expect("parses")
    }

    #[test]
    fn every_example_in_spec_16_2_parses() {
        let one = sftp("thorin:pass@192.168.1.10");
        assert_eq!(one.target.host, "192.168.1.10");
        assert_eq!(one.target.user, "thorin");
        assert_eq!(one.target.port, 22);
        assert_eq!(one.target.dir, None);
        assert_eq!(
            one.password.as_ref().and_then(Secret::expose_str),
            Some("pass")
        );

        let two = sftp("sftp://thorin@nas.local:2222/srv/media");
        assert_eq!(two.target.protocol, Protocol::Sftp);
        assert_eq!(two.target.port, 2222);
        assert_eq!(two.target.dir.as_deref(), Some("/srv/media"));
        assert!(two.password.is_none());

        let three = sftp("ftp://anonymous@ftp.example.org");
        assert_eq!(three.target.protocol, Protocol::Ftp);
        assert_eq!(three.target.port, 21);
        assert_eq!(three.target.user, "anonymous");

        let four = sftp("thorin@buildbox");
        assert_eq!(four.target.host, "buildbox");
        assert_eq!(four.target.user, "thorin");

        let five = sftp("buildbox");
        assert_eq!(five.target.host, "buildbox");
        assert_eq!(five.target.user, "thorin", "$USER for the SSH family");
    }

    #[test]
    fn an_ftp_line_with_no_user_is_anonymous() {
        let parsed = parse("ftp.example.org", Protocol::Ftp, "thorin").expect("parses");
        assert_eq!(parsed.target.user, "anonymous");
        assert!(parsed.password.is_none());
    }

    #[test]
    fn a_bracketed_ipv6_literal_is_a_host_and_not_a_port() {
        let plain = sftp("[::1]");
        assert_eq!(plain.target.host, "[::1]");
        assert_eq!(plain.target.port, 22);

        let ported = sftp("thorin@[fe80::1]:2222/srv");
        assert_eq!(ported.target.host, "[fe80::1]");
        assert_eq!(ported.target.port, 2222);
        assert_eq!(ported.target.dir.as_deref(), Some("/srv"));
    }

    #[test]
    fn a_password_containing_an_at_sign_splits_at_the_last_one() {
        let parsed = sftp("thorin:p@ss@nas.local");
        assert_eq!(parsed.target.host, "nas.local");
        assert_eq!(parsed.target.user, "thorin");
        assert_eq!(
            parsed.password.as_ref().and_then(Secret::expose_str),
            Some("p@ss")
        );
    }

    /// the design puts an initial directory on the line, and `@` is an
    /// ordinary character in a directory name - Synology writes `@eaDir`, npm
    /// writes `@scope`.
    ///
    /// The userinfo split is the authority's, so a `@` in the path is part of
    /// the path. It used to be the whole line's last `@`, which made
    /// `sftp://thorin:pass@nas.local/srv/@scope` parse to host `scope` with
    /// the password `pass@nas.local/srv/` - the typed password offered to a
    /// host the user never named, over FTP with no host key to check
    /// (a password is used for the connection that was asked
    /// for and nothing else).
    #[test]
    fn an_at_sign_in_the_path_is_part_of_the_path() {
        let scoped = sftp("sftp://thorin:pass@nas.local/srv/@scope");
        assert_eq!(scoped.target.host, "nas.local");
        assert_eq!(scoped.target.user, "thorin");
        assert_eq!(scoped.target.dir.as_deref(), Some("/srv/@scope"));
        assert_eq!(
            scoped.password.as_ref().and_then(Secret::expose_str),
            Some("pass")
        );

        // A line with no password does not grow one out of its port and path.
        let ported = sftp("sftp://thorin@nas.local:2222/media@2024");
        assert_eq!(ported.target.host, "nas.local");
        assert_eq!(ported.target.port, 2222);
        assert_eq!(ported.target.user, "thorin");
        assert_eq!(ported.target.dir.as_deref(), Some("/media@2024"));
        assert!(ported.password.is_none());

        let eadir =
            parse("thorin:pass@nas.local/volume1/@eaDir", Protocol::Ftp, "t").expect("parses");
        assert_eq!(eadir.target.host, "nas.local");
        assert_eq!(eadir.target.dir.as_deref(), Some("/volume1/@eaDir"));

        // `redact` reads the line the same way, and only removes the password.
        assert_eq!(
            redact("sftp://thorin:hunter2@nas.local/srv/@scope"),
            "sftp://thorin@nas.local/srv/@scope"
        );
        assert_eq!(
            redact("sftp://thorin@nas.local:2222/media@2024"),
            "sftp://thorin@nas.local:2222/media@2024"
        );
    }

    #[test]
    fn the_refusals_are_each_a_sentence_and_none_quotes_a_password() {
        assert_eq!(parse("", Protocol::Sftp, "t"), Err(ParseError::Empty));
        assert_eq!(
            parse("  ", Protocol::Sftp, "t"),
            Err(ParseError::Empty),
            "whitespace is empty"
        );
        assert_eq!(
            parse("htp://host", Protocol::Sftp, "t"),
            Err(ParseError::UnknownScheme("htp".to_string()))
        );
        assert_eq!(
            parse("nas.local:sixty", Protocol::Sftp, "t"),
            Err(ParseError::BadPort("sixty".to_string()))
        );
        assert_eq!(
            parse("nas.local:0", Protocol::Sftp, "t"),
            Err(ParseError::BadPort("0".to_string())),
            "port zero dials nothing"
        );
        assert_eq!(
            parse("thorin:hunter2@", Protocol::Sftp, "t"),
            Err(ParseError::NoHost)
        );
        let shown = parse("thorin:hunter2@", Protocol::Sftp, "t")
            .expect_err("no host")
            .to_string();
        assert!(!shown.contains("hunter2"), "S3: {shown}");
    }

    #[test]
    fn redact_removes_the_password_and_leaves_everything_else() {
        assert_eq!(
            redact("sftp://thorin:hunter2@nas.local:2222/srv"),
            "sftp://thorin@nas.local:2222/srv"
        );
        assert_eq!(redact("thorin:hunter2@nas.local"), "thorin@nas.local");
        assert_eq!(redact("thorin@nas.local"), "thorin@nas.local");
        assert_eq!(redact("nas.local"), "nas.local");
        // A line that will not parse is still redactable, which is the point.
        assert_eq!(redact("thorin:hunter2@"), "thorin@");
    }

    #[test]
    fn a_parsed_line_never_prints_its_password() {
        let parsed = sftp("sftp://thorin:hunter2@nas.local/srv");
        let shown = format!("{parsed:?}");
        assert!(!shown.contains("hunter2"), "S3: {shown}");
        assert!(!parsed.target.authority().contains("hunter2"));
        assert!(!parsed.target.url("/srv").contains("hunter2"));
    }

    #[test]
    fn the_default_protocol_applies_when_the_line_has_no_scheme() {
        let parsed = parse("mirror.example.org", Protocol::Ftps, "thorin").expect("parses");
        assert_eq!(parsed.target.protocol, Protocol::Ftps);
        assert_eq!(parsed.target.port, 21);
    }
}
