//! `OSC 7` and `OSC 133`, parsed out of the shell's own output.
//!
//! # Why this file exists at all
//!
//! the design rules out reading `/proc/<pid>/cwd` of the shell, "because that
//! races with subprocesses": the pid names the shell, and while `make` is
//! running the directory read back belongs to whatever `make` last chdir'd to.
//! The only thing that can answer "where is the shell now" honestly is the
//! shell itself, and `OSC 7` is how it says so:
//!
//! ```text
//! ESC ] 7 ; file://<host>/<percent-encoded path> BEL
//! ESC ] 7 ; file://<host>/<percent-encoded path> ESC \
//! ```
//!
//! Both terminators are handled, because `vte` - under `vt100`, which parses
//! the PTY stream - dispatches an OSC identically for `BEL` and for `ST`. This
//! module never sees the terminator; it is handed the already-split parameters.
//!
//! # `OSC 133`, and why a second sequence is worth parsing
//!
//! the design makes the command line *the shell's own input line*. Rendering
//! it needs no split between prompt and input - the row is drawn as the shell
//! drew it - but two other things do:
//!
//! * the panel → shell `cd`, which may be written "only when the
//!   shell is at a prompt and its input line is empty".
//! * the decision about the screen, which uses that same test -
//!   "there is one definition of 'the shell is idle' in the program rather than
//!   two that can disagree".
//!
//! `OSC 133 ; A` (prompt start) and `OSC 133 ; B` (input start) mark exactly
//! those two positions, and the shell emits them from the same hook that emits
//! `OSC 7`, so [`crate::console::hooks`] installs all three at once. Because
//! `vte` dispatches escape sequences in stream order, the parser's cursor is
//! *at* the marked position when the callback runs - the split is exact rather
//! than guessed. Where the marks are absent (a shell we do not inject into, or
//! a user who dropped the snippet) both answers degrade to "unknown", and the
//! two features above decline to act rather than acting on a guess.

use std::path::PathBuf;

/// Why an `OSC 7` was not acted on.
///
/// Every variant is a *silent* rejection at the call site - the design says a
/// hostname that is not this machine is ignored - but they are distinct here so
/// the rules can be tested one at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Osc7Reject {
    /// Not an `OSC 7` at all.
    NotOsc7,
    /// The payload was missing or empty.
    Empty,
    /// The payload did not start with `file://`.
    NotAFileUrl,
    /// The authority named a host that is not this machine, so the shell is
    /// somewhere else entirely (the design - ignore it).
    ForeignHost(String),
    /// The path part was empty, or was not absolute once decoded.
    NotAbsolute,
    /// A `%` escape was truncated or not hexadecimal.
    BadEscape,
    /// The decoded bytes are not a path this platform can express.
    NotAPath,
}

impl std::fmt::Display for Osc7Reject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotOsc7 => f.write_str("not an OSC 7"),
            Self::Empty => f.write_str("OSC 7 with no payload"),
            Self::NotAFileUrl => f.write_str("OSC 7 payload is not a file:// URL"),
            Self::ForeignHost(host) => write!(f, "OSC 7 names another host ({host})"),
            Self::NotAbsolute => f.write_str("OSC 7 path is not absolute"),
            Self::BadEscape => f.write_str("OSC 7 has a truncated percent escape"),
            Self::NotAPath => f.write_str("OSC 7 path is not representable here"),
        }
    }
}

/// What an `OSC 133` marked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMark {
    /// `A`: the first cell of the prompt. A multi-line prompt starts here, so
    /// this is what tells the design how many rows the prompt occupies.
    PromptStart,
    /// `B`: the first cell of the *input*, just past the prompt.
    InputStart,
    /// `C`: the command was submitted and its output starts here.
    OutputStart,
    /// `D`: the command finished, optionally with its exit status.
    CommandEnd(Option<i32>),
}

/// Decode an `OSC 7` into the directory it names.
///
/// `params` is what `vte` hands a terminal: the sequence split on `;`, with the
/// leading `7` as the first element. A path containing a literal `;` - legal,
/// and not something every shell snippet percent-encodes - arrives split across
/// several parameters, so everything after the first is rejoined before being
/// decoded.
///
/// `local_host` is this machine's name, as [`hostname`] reports it.
pub fn decode_osc7(params: &[&[u8]], local_host: &str) -> Result<PathBuf, Osc7Reject> {
    let Some((first, rest)) = params.split_first() else {
        return Err(Osc7Reject::NotOsc7);
    };
    if *first != b"7" {
        return Err(Osc7Reject::NotOsc7);
    }
    if rest.is_empty() {
        return Err(Osc7Reject::Empty);
    }
    let payload = rest.join(&b';');
    decode_file_url(&payload, local_host)
}

/// The half of [`decode_osc7`] that is about the URL rather than about the
/// escape sequence. Split out because it is the part worth testing exhaustively.
pub fn decode_file_url(payload: &[u8], local_host: &str) -> Result<PathBuf, Osc7Reject> {
    if payload.is_empty() {
        return Err(Osc7Reject::Empty);
    }
    let rest = payload
        .strip_prefix(b"file://")
        .ok_or(Osc7Reject::NotAFileUrl)?;

    // `file://host/path` - the authority runs to the first `/`, and a payload
    // with no `/` at all has no path.
    let slash = rest
        .iter()
        .position(|b| *b == b'/')
        .ok_or(Osc7Reject::NotAbsolute)?;
    let (host, path) = rest.split_at(slash);
    let host = percent_decode(host)?;
    let host = String::from_utf8_lossy(&host).into_owned();
    if !host_is_local(&host, local_host) {
        return Err(Osc7Reject::ForeignHost(host));
    }

    let decoded = percent_decode(path)?;
    if decoded.first() != Some(&b'/') {
        return Err(Osc7Reject::NotAbsolute);
    }
    to_path(decoded)
}

/// Whether an `OSC 7` authority names this machine.
///
/// Empty and `localhost` are the two spellings of "here" that every shell
/// snippet in the wild uses. Beyond those, the comparison is case-insensitive
/// and made against the first label as well as the whole name, because
/// `$HOSTNAME` is routinely the short form while the kernel's is fully
/// qualified - and the two naming the same machine must not read as a remote
/// shell.
pub fn host_is_local(host: &str, local_host: &str) -> bool {
    if host.is_empty() || host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if host.eq_ignore_ascii_case(local_host) {
        return true;
    }
    let short = |s: &str| s.split('.').next().unwrap_or(s).to_ascii_lowercase();
    !local_host.is_empty() && short(host) == short(local_host)
}

/// Decode `%XX` escapes. A `%` that is not followed by two hex digits is an
/// error rather than a literal: a truncated escape means the sequence was cut
/// off, and half a path is worse than none.
pub fn percent_decode(raw: &[u8]) -> Result<Vec<u8>, Osc7Reject> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        match raw.get(i) {
            Some(b'%') => {
                let hi = raw.get(i.saturating_add(1)).copied().and_then(hex);
                let lo = raw.get(i.saturating_add(2)).copied().and_then(hex);
                match (hi, lo) {
                    (Some(hi), Some(lo)) => {
                        out.push(hi.saturating_mul(16).saturating_add(lo));
                        i = i.saturating_add(3);
                    }
                    _ => return Err(Osc7Reject::BadEscape),
                }
            }
            Some(byte) => {
                out.push(*byte);
                i = i.saturating_add(1);
            }
            None => break,
        }
    }
    Ok(out)
}

const fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Turn decoded bytes into a path.
///
/// On unix a path is bytes, so a filename that is not valid UTF-8 survives
/// intact - the design already records that `VfsPath` is
/// byte-exact while `Entry::name` is lossy, and this keeps that promise for a
/// directory the shell walked into. Elsewhere a path is UTF-16-ish and the only
/// honest answer is to require UTF-8 and reject what is not.
#[cfg(unix)]
fn to_path(bytes: Vec<u8>) -> Result<PathBuf, Osc7Reject> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

#[cfg(not(unix))]
fn to_path(bytes: Vec<u8>) -> Result<PathBuf, Osc7Reject> {
    String::from_utf8(bytes)
        .map(PathBuf::from)
        .map_err(|_| Osc7Reject::NotAPath)
}

/// Decode an `OSC 133` prompt mark, or `None` for anything else.
pub fn decode_osc133(params: &[&[u8]]) -> Option<PromptMark> {
    let (first, rest) = params.split_first()?;
    if *first != b"133" {
        return None;
    }
    let (kind, tail) = rest.split_first()?;
    match *kind {
        b"A" => Some(PromptMark::PromptStart),
        b"B" => Some(PromptMark::InputStart),
        b"C" => Some(PromptMark::OutputStart),
        b"D" => {
            let code = tail
                .first()
                .and_then(|raw| std::str::from_utf8(raw).ok())
                .and_then(|text| text.trim().parse::<i32>().ok());
            Some(PromptMark::CommandEnd(code))
        }
        _ => None,
    }
}

/// This machine's hostname, for [`decode_osc7`]'s authority check.
///
/// No dependency and no `unsafe`: Linux publishes it in `/proc`, every unix
/// puts it in `/etc/hostname`, and the two shells the design names export it
/// as `$HOSTNAME` (bash) or `$HOST` (zsh). An empty answer is not a failure -
/// [`host_is_local`] then accepts only the empty and `localhost` spellings,
/// which is the conservative direction: an unrecognised host is ignored.
pub fn hostname() -> String {
    let from_file = |path: &str| {
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    from_file("/proc/sys/kernel/hostname")
        .or_else(|| from_file("/etc/hostname"))
        .or_else(from_system)
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()))
        .or_else(|| std::env::var("HOST").ok().filter(|s| !s.is_empty()))
        .unwrap_or_default()
}

/// macOS has neither file above, and neither variable is exported to us: a
/// shell sets `HOSTNAME` for itself and does not pass it on. So every answer
/// here was empty, [`host_is_local`] accepted only `localhost`, and a shell
/// that named its host - which `fish` does unprompted - had its `OSC 7`
/// rejected as foreign on every Mac.
///
/// No subprocess and no `unsafe`: sysinfo is already a dependency for the disk
/// list, and its `system` feature is enabled for macOS alone.
#[cfg(target_os = "macos")]
fn from_system() -> Option<String> {
    sysinfo::System::host_name().filter(|s| !s.is_empty())
}

/// Every other target answers from the files above.
#[cfg(not(target_os = "macos"))]
const fn from_system() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn osc7(payload: &str) -> Result<PathBuf, Osc7Reject> {
        let params: Vec<&[u8]> = vec![b"7", payload.as_bytes()];
        decode_osc7(&params, "workstation")
    }

    #[test]
    fn the_ordinary_case() {
        assert_eq!(
            osc7("file://workstation/home/thorin/src"),
            Ok(PathBuf::from("/home/thorin/src"))
        );
    }

    #[test]
    fn an_empty_authority_and_localhost_both_mean_here() {
        assert_eq!(osc7("file:///tmp"), Ok(PathBuf::from("/tmp")));
        assert_eq!(osc7("file://localhost/tmp"), Ok(PathBuf::from("/tmp")));
        assert_eq!(osc7("file://LOCALHOST/tmp"), Ok(PathBuf::from("/tmp")));
    }

    #[test]
    fn the_short_and_long_forms_of_this_host_are_the_same_machine() {
        // $HOSTNAME is routinely the short form while the kernel's is fully
        // qualified. Reading those as two machines would silently switch the
        // cwd sync off on a perfectly ordinary desktop.
        let params: Vec<&[u8]> = vec![b"7", b"file://workstation/srv"];
        assert_eq!(
            decode_osc7(&params, "workstation.example.org"),
            Ok(PathBuf::from("/srv"))
        );
    }

    #[test]
    fn another_host_is_ignored() {
        // an ssh session inside the console is somewhere else, and
        // its cwd means nothing to a local panel.
        assert_eq!(
            osc7("file://build-farm-7/var/tmp"),
            Err(Osc7Reject::ForeignHost("build-farm-7".to_string()))
        );
    }

    #[test]
    fn percent_escapes_are_decoded() {
        assert_eq!(
            osc7("file:///home/thorin/My%20Report%20(final)"),
            Ok(PathBuf::from("/home/thorin/My Report (final)"))
        );
        assert_eq!(
            osc7("file:///tmp/100%25%20done"),
            Ok(PathBuf::from("/tmp/100% done"))
        );
    }

    #[test]
    fn a_truncated_escape_is_refused_rather_than_taken_literally() {
        assert_eq!(osc7("file:///tmp/a%2"), Err(Osc7Reject::BadEscape));
        assert_eq!(osc7("file:///tmp/a%zz"), Err(Osc7Reject::BadEscape));
        assert_eq!(osc7("file:///tmp/a%"), Err(Osc7Reject::BadEscape));
    }

    #[test]
    fn a_semicolon_in_the_path_arrives_split_and_is_rejoined() {
        // vte splits an OSC on `;`, and no rule says a shell percent-encodes
        // one. Without the rejoin this directory would silently become
        // `/tmp/a`.
        let params: Vec<&[u8]> = vec![b"7", b"file:///tmp/a", b"b"];
        assert_eq!(
            decode_osc7(&params, "workstation"),
            Ok(PathBuf::from("/tmp/a;b"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_utf8_survives() {
        // A directory named with a stray 0xff byte is a real directory, and
        // VfsPath is byte-exact.
        use std::os::unix::ffi::OsStrExt;
        let decoded = osc7("file:///tmp/na%ffve").expect("a valid, non-UTF-8 path");
        assert_eq!(decoded.as_os_str().as_bytes(), b"/tmp/na\xffve");
    }

    #[test]
    fn things_that_are_not_an_osc_7_are_refused() {
        let empty: Vec<&[u8]> = Vec::new();
        assert_eq!(decode_osc7(&empty, "h"), Err(Osc7Reject::NotOsc7));
        let other: Vec<&[u8]> = vec![b"9", b"file:///tmp"];
        assert_eq!(decode_osc7(&other, "h"), Err(Osc7Reject::NotOsc7));
        let bare: Vec<&[u8]> = vec![b"7"];
        assert_eq!(decode_osc7(&bare, "h"), Err(Osc7Reject::Empty));
        assert_eq!(osc7("/home/thorin"), Err(Osc7Reject::NotAFileUrl));
        assert_eq!(osc7("file://host-only"), Err(Osc7Reject::NotAbsolute));
    }

    #[test]
    fn prompt_marks() {
        let mark = |p: Vec<&[u8]>| decode_osc133(&p);
        assert_eq!(mark(vec![b"133", b"A"]), Some(PromptMark::PromptStart));
        assert_eq!(mark(vec![b"133", b"B"]), Some(PromptMark::InputStart));
        assert_eq!(mark(vec![b"133", b"C"]), Some(PromptMark::OutputStart));
        assert_eq!(
            mark(vec![b"133", b"D", b"130"]),
            Some(PromptMark::CommandEnd(Some(130)))
        );
        assert_eq!(mark(vec![b"133", b"D"]), Some(PromptMark::CommandEnd(None)));
        assert_eq!(mark(vec![b"7", b"file:///tmp"]), None);
        assert_eq!(mark(vec![b"133"]), None);
    }
}
