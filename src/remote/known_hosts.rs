//! `~/.ssh/known_hosts`, and the three answers it can give.
//!
//! > Verify against `~/.ssh/known_hosts` using the standard format, including
//! > hashed entries. On an unknown host, show the fingerprint and ask; on a
//! > **changed** host key, refuse loudly and do not offer a one-key override -
//! > that is the one case where friction is the feature. Never default to
//! > accepting any key.
//!
//! # Why there is no way to say yes to a changed key
//!
//! [`Verdict`] has no variant that means "changed, but allowed", and
//! [`Verdict::Changed`] carries no token, handle or callback that could be
//! turned into one. [`learn`] refuses a changed or revoked key itself rather
//! than trusting its caller to have looked, so a mistake in the connect task
//! cannot append over a key that changed. There is no configuration value and
//! no environment variable that changes any of this: `remote.strict_host_keys`
//! exists, defaults to true, and narrows the *unknown* case in a
//! non-interactive context - it cannot widen anything.
//!
//!
//! # Who parses what, and why it is split
//!
//! The hashed-hostname form (`|1|salt|hash`) is HMAC-SHA1 over the host name.
//! Computing it here would need `hmac` and `sha1` as direct dependencies and
//! the table does not list them, so the HMAC is `russh`'s: this
//! module asks [`russh::keys::known_hosts::known_host_keys_path`] which lines
//! of the file match, and uses the answer for the hashed lines only. A hashed
//! line is never skipped - skipping one would silently downgrade every user who
//! has run `ssh-keygen -H` to "unknown host" on every connect, which is how a
//! person learns to click through the prompt.
//!
//! Everything else is here, because `russh`'s walk does not do it: it has no
//! wildcards, no negations and no marker handling, and its line counter does
//! not advance over comment lines. This module therefore reads the file itself
//! with `ssh-key`'s `known_hosts` parser (a `russh` re-export, not a new
//! dependency), owns the pattern matching and the `@revoked` and
//! `@cert-authority` markers, and consults `russh` only for the question it
//! cannot answer without a crate the spec does not allow.
//!
//! # `@cert-authority` is not implemented, and says so
//!
//! Certificate host keys are out of scope for this milestone. An entry marked
//! `@cert-authority` is not a host key, so it is not compared against the key
//! the server offered: a host whose only entry is a CA entry is
//! [`Verdict::Unknown`] and the user is shown the fingerprint. That is a
//! prompt, never a silent accept.

use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use russh::keys::PublicKey;
use russh::keys::ssh_key::HashAlg;
use russh::keys::ssh_key::known_hosts::{Entry as HostEntry, HostPatterns, Marker};

use super::Target;
use crate::config::paths::home_dir;
use crate::error::{Error, Result};

/// What `~/.ssh/known_hosts` says about a host key.
///
/// Four outcomes and no fifth. Note what is **not** here: there is no
/// `Verdict` that means "changed, but allowed", and [`Verdict::Changed`]
/// carries no way to proceed. the "do not offer a one-key
/// override" is enforced by the shape of this enum, not by a policy check that
/// somebody could invert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// This exact key is already trusted for this host.
    Known,
    /// No key at all for this host: show the fingerprint and ask.
    ///
    Unknown {
        /// The fingerprint of the key the server offered, to be shown.
        fingerprint: String,
    },
    /// A **different** key is recorded for this host. Refuse loudly.
    Changed {
        /// Which line of the file records the other key, so the user can look.
        line: usize,
        /// The fingerprint of the key the server offered **now**. The recorded
        /// one is at `line` of the file, and [`changed_lines`] reads it back.
        fingerprint: String,
    },
    /// The key is marked `@revoked`. Refuse, in the same words as
    /// [`Verdict::Changed`].
    Revoked {
        /// Which line of the file revokes it.
        line: usize,
    },
}

/// `~/.ssh/known_hosts`, honouring `$HOME`.
pub fn path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".ssh").join("known_hosts"))
}

/// One line of the file that parsed.
///
/// Lines that do not parse are skipped rather than failing the whole file:
/// `known_hosts` is hand-edited, it accumulates entries from years of `ssh`
/// versions, and one bad line must not lock a user out of every host. What a
/// skipped line cannot do is make a key *more* trusted, because trust here
/// comes only from a line that parsed and matched.
struct Recorded {
    /// The 1-based line number, counting every line in the file.
    line: usize,
    /// `@revoked` or `@cert-authority`, when the line carries one.
    marker: Option<Marker>,
    /// The host field, already parsed into patterns or a hash.
    patterns: HostPatterns,
    /// The key on that line.
    key: PublicKey,
    /// Whether [`Recorded::patterns`] is the hashed form, which only `russh`
    /// can match.
    hashed: bool,
    /// Set when `russh` says this hashed line matches the host being verified.
    matched_hashed: bool,
}

/// Verify one key against a `known_hosts` **file**.
///
/// A file that is not there is [`Verdict::Unknown`]: a first connection on a
/// new machine is not an error. A file that cannot be read is an error, because
/// treating "permission denied" as "no key recorded" would turn a locked-down
/// home directory into a prompt to trust anything.
pub fn verify(file: &Path, host: &str, port: u16, key: &PublicKey) -> Result<Verdict> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Verdict::Unknown {
                fingerprint: fingerprint(key),
            });
        }
        Err(err) => return Err(Error::io(file, err)),
    };

    let mut recorded = parse(&text);
    mark_hashed_matches(file, host, port, &mut recorded)?;

    let candidate = lookup_name(host, port);
    let matched: Vec<&Recorded> = recorded
        .iter()
        .filter(|entry| {
            if entry.hashed {
                entry.matched_hashed
            } else {
                patterns_match(&entry.patterns, &candidate)
            }
        })
        .collect();

    // Revocation first, and over the whole file rather than over the lines that
    // matched this host. Two reasons, and both point the same way. A revoked
    // key is a key somebody decided is not to be trusted, and trusting it here
    // because the line names another host is a distinction a person who wrote
    // `@revoked` did not intend. And a hashed `@revoked` line cannot be
    // host-matched at all without the HMAC this module does not have: `russh`
    // skips a line whose first field is a marker, so matching it here would be
    // a match that never happens. (`ssh-keygen -H` leaves marker lines
    // unhashed, verified on this machine, so the hand-written case is the only
    // one this covers - and it covers it by refusing.)
    for entry in &recorded {
        if entry.marker == Some(Marker::Revoked) && &entry.key == key {
            return Ok(Verdict::Revoked { line: entry.line });
        }
    }

    // A marker line is not a host key: `@cert-authority` names a signer, and
    // comparing the server's key against it would report a change that did not
    // happen.
    let plain: Vec<&&Recorded> = matched
        .iter()
        .filter(|entry| entry.marker.is_none())
        .collect();

    if plain.iter().any(|entry| &entry.key == key) {
        return Ok(Verdict::Known);
    }

    // Only a key of the same type is a change. A host that has an ed25519 key
    // recorded and offers its RSA one has not changed its ed25519 key, and
    // `ssh` calls that an unknown key of a new type rather than an alarm.
    if let Some(entry) = plain
        .iter()
        .find(|entry| entry.key.algorithm() == key.algorithm())
    {
        return Ok(Verdict::Changed {
            line: entry.line,
            fingerprint: fingerprint(key),
        });
    }

    Ok(Verdict::Unknown {
        fingerprint: fingerprint(key),
    })
}

/// Every line of the file that parsed, with its 1-based line number.
fn parse(text: &str) -> Vec<Recorded> {
    let mut recorded = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        // `ssh-key`'s own reader strips at the first `#` and trims the tail;
        // this does the same so that the two agree about what a line is.
        let line = match raw.split_once('#') {
            Some((before, _)) => before,
            None => raw,
        }
        .trim_end();
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = line.parse::<HostEntry>() else {
            continue;
        };
        let hashed = matches!(entry.host_patterns(), HostPatterns::HashedName { .. });
        recorded.push(Recorded {
            line: index + 1,
            marker: entry.marker().copied(),
            patterns: entry.host_patterns().clone(),
            key: entry.public_key().clone(),
            hashed,
            matched_hashed: false,
        });
    }
    recorded
}

/// Ask `russh` which hashed lines match, and mark them.
///
/// `russh` returns `(line, key)` pairs for every line whose host field matched,
/// hashed or not. Its line numbers are used when they agree with this module's,
/// which they do for any file with no comment lines above the entry, and the
/// key is used to attribute the match otherwise. Only hashed lines are marked
/// from this answer: the plain ones are matched here, with the wildcards and
/// negations `russh` does not implement.
///
/// An error from `russh` is an error here, never a shrug: a file it cannot walk
/// is a file whose hashed entries cannot be checked, and continuing would
/// downgrade a known host to an unknown one and put a prompt on the screen.
fn mark_hashed_matches(
    file: &Path,
    host: &str,
    port: u16,
    recorded: &mut [Recorded],
) -> Result<()> {
    if !recorded.iter().any(|entry| entry.hashed) {
        return Ok(());
    }
    let lowered = host.to_ascii_lowercase();
    let mut names = vec![host.to_string()];
    if lowered != host {
        // `ssh-keygen -H` hashes the name as it was written. Trying the
        // lower-cased form as well costs one pass over a small file and stops a
        // host typed in capitals from silently becoming an unknown one.
        names.push(lowered);
    }
    let mut answers = Vec::new();
    for name in names {
        let found =
            russh::keys::known_hosts::known_host_keys_path(&name, port, file).map_err(|err| {
                Error::msg(format!(
                    "{}: this file could not be read as known_hosts: {err}",
                    file.display()
                ))
            })?;
        answers.extend(found);
    }

    for (line, key) in answers {
        // The line numbers agree unless the file has comment lines in it, so
        // try that first and fall back to the key.
        let exact = recorded
            .iter_mut()
            .find(|entry| entry.line == line && entry.key == key);
        match exact {
            Some(entry) if entry.hashed => {
                entry.matched_hashed = true;
                continue;
            }
            // A plain line, matched by `russh` as well as by this module. Its
            // verdict is decided here, so there is nothing to mark.
            Some(_) => continue,
            None => {}
        }
        for entry in recorded.iter_mut() {
            if entry.hashed && entry.key == key {
                entry.matched_hashed = true;
            }
        }
    }
    Ok(())
}

/// The name a `known_hosts` lookup is made under: the host itself on port 22,
/// and `[host]:port` on any other, which is the form `ssh` writes and reads.
fn lookup_name(host: &str, port: u16) -> String {
    let host = host.to_ascii_lowercase();
    if port == 22 {
        host
    } else {
        format!("[{host}]:{port}")
    }
}

/// Whether a non-hashed host field matches the name being looked up.
///
/// OpenSSH's rules: a comma-separated list, `*` and `?` as wildcards, and a
/// leading `!` negating a pattern. One negated pattern that matches rejects the
/// whole entry however many positive ones also match, which is what makes
/// `!secure.local,*.local` mean what it reads as.
fn patterns_match(patterns: &HostPatterns, name: &str) -> bool {
    let list = match patterns {
        HostPatterns::Patterns(list) => list,
        // Hashed entries are `russh`'s to match; this is never reached because
        // the caller checks `Recorded::hashed` first.
        HostPatterns::HashedName { .. } => return false,
    };
    let mut positive = false;
    for pattern in list {
        match pattern.strip_prefix('!') {
            Some(negated) => {
                if glob(negated, name) {
                    return false;
                }
            }
            None => {
                if glob(pattern, name) {
                    positive = true;
                }
            }
        }
    }
    positive
}

/// `*` and `?` against one name, case-insensitively, with no recursion and no
/// backtracking blow-up: one pass forward, remembering the last `*`.
fn glob(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let name: Vec<char> = name.to_ascii_lowercase().chars().collect();
    let (mut at_pattern, mut at_name) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while at_name < name.len() {
        let here = pattern.get(at_pattern).copied();
        let now = name.get(at_name).copied();
        match here {
            Some('*') => {
                star = Some((at_pattern, at_name));
                at_pattern += 1;
            }
            Some('?') => {
                at_pattern += 1;
                at_name += 1;
            }
            Some(ch) if Some(ch) == now => {
                at_pattern += 1;
                at_name += 1;
            }
            _ => match star {
                Some((star_at, from)) => {
                    at_pattern = star_at + 1;
                    at_name = from + 1;
                    star = Some((star_at, from + 1));
                }
                None => return false,
            },
        }
    }
    while pattern.get(at_pattern) == Some(&'*') {
        at_pattern += 1;
    }
    at_pattern == pattern.len()
}

/// Append a newly accepted key, creating the file `0600` and the directory
/// `0700` if they do not exist.
///
/// It verifies before it writes, and refuses [`Verdict::Changed`] and
/// [`Verdict::Revoked`] itself. The connect task only ever calls this from the
/// `Accept` arm of the unknown-host prompt, and this check is the second lock
/// on that door: no sequence of calls from anywhere in the program can turn a
/// changed key into a recorded one.
///
/// A key that is already recorded is left alone rather than appended twice.
pub fn learn(file: &Path, host: &str, port: u16, key: &PublicKey) -> Result<()> {
    match verify(file, host, port, key)? {
        Verdict::Known => return Ok(()),
        Verdict::Unknown { .. } => {}
        Verdict::Changed { line, .. } => {
            return Err(Error::msg(format!(
                "{}: line {line} records a different key for this host; it is not overwritten",
                file.display()
            )));
        }
        Verdict::Revoked { line } => {
            return Err(Error::msg(format!(
                "{}: line {line} revokes this key; it is not recorded",
                file.display()
            )));
        }
    }

    let openssh = key
        .to_openssh()
        .map_err(|err| Error::msg(format!("this host key could not be written down: {err}")))?;
    // One line, whatever the key's comment field holds: a newline in it would
    // otherwise write two entries, one of them nonsense.
    let Some(first) = openssh.lines().next().filter(|line| !line.is_empty()) else {
        return Err(Error::msg("this host key could not be written down"));
    };

    if let Some(parent) = file.parent()
        && !parent.exists()
    {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|err| Error::io(parent, err))?;
    }

    use std::os::unix::fs::OpenOptionsExt as _;
    let mut handle = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        // Only applied when this call creates the file. `known_hosts` is not
        // secret, but it is the record of what this account trusts, and a file
        // another user can append to is a file that can add a host key.
        .mode(0o600)
        .open(file)
        .map_err(|err| Error::io(file, err))?;

    if needs_newline(&mut handle).map_err(|err| Error::io(file, err))? {
        handle
            .write_all(b"\n")
            .map_err(|err| Error::io(file, err))?;
    }
    let name = lookup_name(host, port);
    writeln!(handle, "{name} {first}").map_err(|err| Error::io(file, err))?;
    handle.flush().map_err(|err| Error::io(file, err))
}

/// Whether the file needs a newline before something is appended to it.
///
/// A `known_hosts` whose last line has no terminator would otherwise get the
/// new host glued onto the end of the old one, which loses both.
fn needs_newline(handle: &mut std::fs::File) -> std::io::Result<bool> {
    let length = handle.metadata()?.len();
    if length == 0 {
        return Ok(false);
    }
    handle.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    handle.read_exact(&mut last)?;
    Ok(last.first().copied() != Some(b'\n'))
}

/// `SHA256:cSZl24ZIbs09gyHUOKCL81rlk8QGx/vH2e/T7WPcEuk`, exactly as `ssh` and
/// `ssh-keygen -l` render it, so a user can compare the two strings character
/// by character. Base64, unpadded.
pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// The lines the unknown-host prompt shows.
///
/// The fingerprint is on a line of its own so it can be compared with the one
/// the person who runs the host read out, and the last line says what accepting
/// means. Nothing here carries a secret: a [`Target`] has none to carry.
pub fn unknown_lines(target: &Target, fingerprint: &str) -> Vec<String> {
    vec![
        format!("{} is not in known_hosts.", target.authority()),
        "The host offered this key:".to_string(),
        format!("  {fingerprint}"),
        "Accept it only if that is the fingerprint you expect. Accepting records".to_string(),
        "the key and trusts this host from now on.".to_string(),
    ]
}

/// The lines the changed-key refusal shows.
///
/// It names the file and the line number so the user can go and look, both
/// fingerprints so they can be compared, and it says what to do - which is to
/// verify the new key out of band, not to delete the line because a program
/// told them to. There is no affirmative button on this dialog and this text
/// does not describe one.
///
/// [`Verdict::Known`] and [`Verdict::Unknown`] have no refusal to show and
/// return no lines: they are not this function's question, and answering them
/// with a sentence would put a refusal on the screen for a host that was fine.
pub fn changed_lines(target: &Target, verdict: &Verdict, file: &Path) -> Vec<String> {
    let authority = target.authority();
    match verdict {
        Verdict::Known | Verdict::Unknown { .. } => Vec::new(),
        Verdict::Changed { line, fingerprint } => {
            let mut lines = vec![
                format!("THE HOST KEY FOR {authority} HAS CHANGED."),
                String::new(),
                "The host offered this key:".to_string(),
                format!("  {fingerprint}"),
            ];
            if let Some(recorded) = recorded_fingerprint(file, *line) {
                lines.push(format!("Line {line} of the file records:"));
                lines.push(format!("  {recorded}"));
            }
            lines.push(format!("  {}, line {line}", file.display()));
            lines.push(String::new());
            lines.push(
                "This can mean the host was rebuilt. It can also mean something is".to_string(),
            );
            lines.push("between you and it, reading everything you send.".to_string());
            lines.push(
                "Verify the new key with whoever runs the host before you change the".to_string(),
            );
            lines.push("file. There is no override here.".to_string());
            lines
        }
        Verdict::Revoked { line } => vec![
            format!("THE HOST KEY FOR {authority} IS REVOKED."),
            String::new(),
            format!("  {}, line {line}", file.display()),
            String::new(),
            "The key this host offered is marked @revoked in known_hosts, which".to_string(),
            "means it is not to be trusted again. There is no override here".to_string(),
            ".".to_string(),
        ],
    }
}

/// The fingerprint of the key recorded on one line of a file, for the message
/// above. Best effort: a file that has changed since the verdict, or a line
/// that no longer parses, simply leaves that row out of the dialog.
fn recorded_fingerprint(file: &Path, line: usize) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    let raw = text.lines().nth(line.checked_sub(1)?)?;
    let trimmed = match raw.split_once('#') {
        Some((before, _)) => before,
        None => raw,
    }
    .trim_end();
    let entry = trimmed.parse::<HostEntry>().ok()?;
    Some(fingerprint(entry.public_key()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::Protocol;

    /// The key the hashed fixtures below record, and the one `ssh-keygen -l`
    /// on this machine printed the fingerprint of.
    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINaKqpzO5HsBtz86LK7ncXs3pheMnlfXW0gJR/4S0SZb";
    /// A different ed25519 key, for the changed case.
    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJLnVqBi8dDrqFKh3pdpOL0pubb3a1UnX5MkigBX8iB1";
    /// An RSA key for the same host, for the "two types, both known" case.
    const KEY_C: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQCmrODl0gt5E2yralX3Uxhm3YKnKRxmMuI8uoz7wHLGQxwCpHGzzejdWyE6h/KyY4KEsldw7+rg6N/yaYNPBDd/QqzivZVQrFuqDnc4QCS0JazpNrk9y4WyEWl9VWCqm+kx+vK4MkYLwfYNkFN/owKnrDIwQbFwBTdsBJuXgBsShFJTyM6vE/nAXeQhO+CLWWSFs9MgCsXOi3rSequoeIO5knPVibte+QDXz5gHv27EQAq4zc6BPE6XnMrtj48mHgX8xEX2vKHoHAnQ6KZBtBvrdno4TSerjLDROee0ZSYCuf0dGe+XRTGIoUriqdaqBnEvuvONctQBtiFGsx9NqsZZ";
    /// `ssh-keygen -l` on this machine, for [`KEY_A`]. Compared character for
    /// character: a fingerprint a user cannot compare with `ssh`'s is useless.
    const FINGERPRINT_A: &str = "SHA256:cSZl24ZIbs09gyHUOKCL81rlk8QGx/vH2e/T7WPcEuk";

    /// `ssh-keygen -H` on this machine, over [`KEY_A`], for `nas.local`.
    /// Verified with `ssh-keygen -f <file> -F nas.local`, which found it.
    const HASHED_22: &str = "|1|pnzoWYm0NTPPBksWUKjpjhEdFcQ=|tzjgzqAPTrR5NI6XTYSFYbXnDfg= ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINaKqpzO5HsBtz86LK7ncXs3pheMnlfXW0gJR/4S0SZb";
    /// The same, for `[nas.local]:2222`.
    const HASHED_2222: &str = "|1|X1AO7e0+llgoGr8yKUQB/Uji+w8=|g0cLBMkwLRBn9mqnDj4SL9bOZus= ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINaKqpzO5HsBtz86LK7ncXs3pheMnlfXW0gJR/4S0SZb";

    /// [`verify`] for a test that knows the file is readable. `Error` has no
    /// `PartialEq`, so a `Result` cannot be compared with `assert_eq!`.
    fn verdict(file: &Path, host: &str, port: u16, key: &PublicKey) -> Verdict {
        verify(file, host, port, key).expect("verify reads the fixture")
    }

    fn key(text: &str) -> PublicKey {
        text.parse::<PublicKey>().expect("a fixture key parses")
    }

    /// A `known_hosts` in its own directory, named after the test.
    fn fixture(name: &str, contents: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hcmd-kh-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the fixture directory");
        let file = dir.join("known_hosts");
        std::fs::write(&file, contents).expect("write the fixture");
        file
    }

    fn target(port: u16) -> Target {
        Target {
            protocol: Protocol::Sftp,
            host: "nas.local".to_string(),
            port,
            user: "thorin".to_string(),
            dir: None,
        }
    }

    #[test]
    fn the_fingerprint_is_what_ssh_keygen_prints() {
        assert_eq!(fingerprint(&key(KEY_A)), FINGERPRINT_A);
    }

    #[test]
    fn a_plain_line_is_known() {
        let file = fixture("plain", &format!("nas.local {KEY_A}\n"));
        assert_eq!(verdict(&file, "nas.local", 22, &key(KEY_A)), Verdict::Known);
    }

    /// for an IPv6 host: the lookup name is the one `ssh`
    /// writes, `[::1]:2222`, and never the doubly bracketed `[[::1]]:2222`.
    ///
    /// The quick-connect line keeps an IPv6 literal's brackets, and
    /// [`lookup_name`] adds its own pair, so passing `Target::host` through
    /// here would match no entry a user already has - an unknown-host prompt
    /// on every connection, which is how a user learns to click Accept
    /// without reading. `Target::hostname` is what the SFTP handler passes.
    #[test]
    fn an_ipv6_host_is_looked_up_the_way_ssh_writes_it() {
        let file = fixture("ipv6", &format!("[::1]:2222 {KEY_A}\n"));
        assert_eq!(verdict(&file, "::1", 2222, &key(KEY_A)), Verdict::Known);
        assert!(
            matches!(
                verdict(&file, "[::1]", 2222, &key(KEY_A)),
                Verdict::Unknown { .. }
            ),
            "a doubly bracketed name matches nothing, which is why the \
             backend unbrackets before it asks"
        );
        // And what this program writes is readable by `ssh` for the same host.
        let written = fixture("ipv6-learn", "");
        learn(&written, "::1", 2222, &key(KEY_A)).expect("learn");
        let text = std::fs::read_to_string(&written).expect("the file");
        assert!(text.starts_with("[::1]:2222 ssh-ed25519 "), "{text}");
        assert_eq!(verdict(&written, "::1", 2222, &key(KEY_A)), Verdict::Known);
    }

    #[test]
    fn a_bracketed_line_matches_only_its_own_port() {
        let file = fixture("port", &format!("[nas.local]:2222 {KEY_A}\n"));
        assert_eq!(
            verdict(&file, "nas.local", 2222, &key(KEY_A)),
            Verdict::Known
        );
        assert!(matches!(
            verdict(&file, "nas.local", 22, &key(KEY_A)),
            Verdict::Unknown { .. }
        ));
    }

    #[test]
    fn a_comma_separated_host_list_matches_each_of_them() {
        let file = fixture("list", &format!("nas.local,192.168.1.10 {KEY_A}\n"));
        assert_eq!(verdict(&file, "nas.local", 22, &key(KEY_A)), Verdict::Known);
        assert_eq!(
            verdict(&file, "192.168.1.10", 22, &key(KEY_A)),
            Verdict::Known
        );
        assert!(matches!(
            verdict(&file, "other.local", 22, &key(KEY_A)),
            Verdict::Unknown { .. }
        ));
    }

    #[test]
    fn a_wildcard_matches_the_hosts_under_it() {
        let file = fixture("wildcard", &format!("*.local {KEY_A}\n"));
        assert_eq!(verdict(&file, "nas.local", 22, &key(KEY_A)), Verdict::Known);
        assert!(matches!(
            verdict(&file, "nas.example.org", 22, &key(KEY_A)),
            Verdict::Unknown { .. }
        ));
    }

    #[test]
    fn a_negated_pattern_takes_a_host_back_out_of_a_wildcard() {
        let file = fixture("negation", &format!("!secure.local,*.local {KEY_A}\n"));
        assert_eq!(verdict(&file, "nas.local", 22, &key(KEY_A)), Verdict::Known);
        assert!(
            matches!(
                verdict(&file, "secure.local", 22, &key(KEY_A)),
                Verdict::Unknown { .. }
            ),
            "a negated pattern rejects the whole entry"
        );
    }

    #[test]
    fn a_hashed_entry_is_matched_and_never_skipped() {
        let file = fixture("hashed", &format!("{HASHED_22}\n"));
        assert_eq!(
            verdict(&file, "nas.local", 22, &key(KEY_A)),
            Verdict::Known,
            "a hashed entry must not silently become an unknown host"
        );
        assert!(matches!(
            verdict(&file, "other.local", 22, &key(KEY_A)),
            Verdict::Unknown { .. }
        ));
    }

    #[test]
    fn a_hashed_entry_for_a_port_is_matched_on_that_port() {
        let file = fixture("hashed-port", &format!("{HASHED_2222}\n"));
        assert_eq!(
            verdict(&file, "nas.local", 2222, &key(KEY_A)),
            Verdict::Known
        );
        assert!(matches!(
            verdict(&file, "nas.local", 22, &key(KEY_A)),
            Verdict::Unknown { .. }
        ));
    }

    #[test]
    fn a_hashed_entry_is_found_behind_comment_lines() {
        // `russh`'s own line counter does not advance over a comment, so this
        // is the case the key-based attribution in `mark_hashed_matches`
        // exists for. Two comments, so a line-number coincidence cannot pass.
        let file = fixture(
            "hashed-comments",
            &format!("# a comment\n# another\n{HASHED_22}\n"),
        );
        assert_eq!(verdict(&file, "nas.local", 22, &key(KEY_A)), Verdict::Known);
    }

    /// A file where **every** line is hashed, which is what a machine that has
    /// run `ssh-keygen -H` actually looks like.
    ///
    /// Produced on this machine: four plain entries (`nas.local`,
    /// `[buildbox]:2222`, `mirror.example.org,192.168.1.44`, and a second key
    /// type for `nas.local`) written with two throwaway host keys and then
    /// hashed with `ssh-keygen -H`, which splits the comma-separated line into
    /// one hashed line per host because a hash cannot hold a list.
    /// `ssh-keygen -F` on the result finds two entries for `nas.local`, one
    /// each for `[buildbox]:2222`, `mirror.example.org` and `192.168.1.44`,
    /// and none for `other.local`. These assertions are that oracle.
    #[test]
    fn a_wholly_hashed_file_answers_the_way_ssh_keygen_does() {
        const ED: &str =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIP7DIVjl/sGY94UogaZknb9pa5jLiDBPG86STe7yIQ4o";
        const ECDSA: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBAaleUAvqlcanzFJDXzo/P1XTMEFLvKhnmrm4cuEgWCvmTEMAiH6dIWDsAwGZe3LdzHxVmnqHwFvyoHF+4DRXSg=";
        let file = fixture(
            "all-hashed",
            &format!(
                "|1|MHweDx76MAlrJxV2BDA+QxKqcZA=|t6YOriZcZeDX2MD3r4ocMNn3Jd0= {ED}\n\
                 |1|wH1vvIMQ88uylkykbDYOWyNFk2k=|5gzpwfeMU4/dm6zn3ls913vFk/A= {ED}\n\
                 |1|kJpTc4l4J6A07QmW9GFw6JiUBEI=|rWPs6K0iMvV+3e/2j2Wf/a2O9YM= {ECDSA}\n\
                 |1|Ej8iWBd7MafdhEmqBnbOG56P/pU=|d83Dl/fnbzPBI/tUUEuKBio4FKY= {ECDSA}\n\
                 |1|tACuJ28bZIOuHdtnrAGbHtrDnxA=|luNfkED53ecJ+5XOwBgjKleNcHY= {ECDSA}\n"
            ),
        );
        assert_eq!(verdict(&file, "nas.local", 22, &key(ED)), Verdict::Known);
        assert_eq!(
            verdict(&file, "nas.local", 22, &key(ECDSA)),
            Verdict::Known,
            "the second hashed line for the same host is a second key, not a change"
        );
        assert_eq!(verdict(&file, "buildbox", 2222, &key(ED)), Verdict::Known);
        assert_eq!(
            verdict(&file, "mirror.example.org", 22, &key(ECDSA)),
            Verdict::Known
        );
        assert_eq!(
            verdict(&file, "192.168.1.44", 22, &key(ECDSA)),
            Verdict::Known,
            "ssh-keygen -H writes one hashed line per host of a comma list"
        );
        assert!(matches!(
            verdict(&file, "other.local", 22, &key(ED)),
            Verdict::Unknown { .. }
        ));
        assert!(
            matches!(
                verdict(&file, "buildbox", 22, &key(ED)),
                Verdict::Unknown { .. }
            ),
            "the hashed entry is for [buildbox]:2222 and not for port 22"
        );
        // And a different key of a type that is recorded is still a change,
        // through the hashed path as much as the plain one.
        assert!(matches!(
            verdict(&file, "nas.local", 22, &key(KEY_B)),
            Verdict::Changed { .. }
        ));
    }

    #[test]
    fn a_changed_hashed_key_is_changed_and_not_unknown() {
        let file = fixture("hashed-changed", &format!("{HASHED_22}\n"));
        let outcome = verdict(&file, "nas.local", 22, &key(KEY_B));
        assert!(
            matches!(outcome, Verdict::Changed { line: 1, .. }),
            "got {outcome:?}"
        );
    }

    #[test]
    fn a_changed_key_is_refused_and_names_its_line() {
        let file = fixture(
            "changed",
            &format!("# a note\n\nnas.local {KEY_A}\nother.local {KEY_B}\n"),
        );
        let outcome = verdict(&file, "nas.local", 22, &key(KEY_B));
        assert_eq!(
            outcome,
            Verdict::Changed {
                line: 3,
                fingerprint: fingerprint(&key(KEY_B)),
            },
            "the line number is the one a user counts in an editor"
        );
    }

    #[test]
    fn two_key_types_for_one_host_are_both_known() {
        let file = fixture(
            "two-types",
            &format!("nas.local {KEY_A}\nnas.local {KEY_C}\n"),
        );
        assert_eq!(verdict(&file, "nas.local", 22, &key(KEY_A)), Verdict::Known);
        assert_eq!(
            verdict(&file, "nas.local", 22, &key(KEY_C)),
            Verdict::Known,
            "a second key of another type is not a changed key"
        );
    }

    #[test]
    fn a_revoked_key_is_refused_even_where_another_line_trusts_it() {
        let file = fixture(
            "revoked",
            &format!("nas.local {KEY_A}\n@revoked nas.local {KEY_A}\n"),
        );
        assert_eq!(
            verdict(&file, "nas.local", 22, &key(KEY_A)),
            Verdict::Revoked { line: 2 }
        );
    }

    #[test]
    fn a_revoked_key_is_refused_even_where_the_line_names_another_host() {
        let file = fixture(
            "revoked-elsewhere",
            &format!("nas.local {KEY_A}\n@revoked other.local {KEY_A}\n"),
        );
        assert_eq!(
            verdict(&file, "nas.local", 22, &key(KEY_A)),
            Verdict::Revoked { line: 2 },
            "a key somebody revoked is not trusted here because the line names \
             a different host"
        );
    }

    #[test]
    fn a_hashed_revoked_line_still_refuses() {
        // Hand written: `ssh-keygen -H` leaves marker lines unhashed, so this
        // form only ever comes from a person. `russh` skips it, so the
        // file-wide revocation check in `verify` is the only thing that sees
        // it, and this is that check.
        let file = fixture("revoked-hashed", &format!("@revoked {HASHED_22}\n"));
        assert_eq!(
            verdict(&file, "nas.local", 22, &key(KEY_A)),
            Verdict::Revoked { line: 1 }
        );
    }

    #[test]
    fn a_cert_authority_line_is_not_a_host_key() {
        let file = fixture("ca", &format!("@cert-authority *.local {KEY_A}\n"));
        assert!(
            matches!(
                verdict(&file, "nas.local", 22, &key(KEY_A)),
                Verdict::Unknown { .. }
            ),
            "a CA entry is a signer, not the host's own key: ask rather than accept"
        );
        assert!(
            matches!(
                verdict(&file, "nas.local", 22, &key(KEY_B)),
                Verdict::Unknown { .. }
            ),
            "and it is not a changed key either"
        );
    }

    #[test]
    fn comments_blank_lines_and_trailing_whitespace_are_ignored() {
        let file = fixture(
            "noise",
            &format!("# a comment\n\n   \nnas.local {KEY_A}   \n# trailing comment\n"),
        );
        assert_eq!(verdict(&file, "nas.local", 22, &key(KEY_A)), Verdict::Known);
    }

    #[test]
    fn a_line_that_does_not_parse_does_not_break_the_file() {
        let file = fixture(
            "broken",
            &format!("this is not a known_hosts line\nnas.local {KEY_A}\n"),
        );
        assert_eq!(verdict(&file, "nas.local", 22, &key(KEY_A)), Verdict::Known);
    }

    #[test]
    fn a_missing_file_is_an_unknown_host_and_not_an_error() {
        let dir = std::env::temp_dir().join(format!("hcmd-kh-{}-absent", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("known_hosts");
        assert_eq!(
            verdict(&file, "nas.local", 22, &key(KEY_A)),
            Verdict::Unknown {
                fingerprint: FINGERPRINT_A.to_string()
            }
        );
    }

    #[test]
    fn learning_a_key_creates_the_file_private_and_the_directory_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("hcmd-kh-{}-learn", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join(".ssh").join("known_hosts");
        learn(&file, "nas.local", 2222, &key(KEY_A)).expect("learn");

        let file_mode = std::fs::metadata(&file).expect("stat").permissions().mode();
        assert_eq!(file_mode & 0o777, 0o600);
        let dir_mode = std::fs::metadata(file.parent().expect("parent"))
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700);

        let written = std::fs::read_to_string(&file).expect("read back");
        assert!(written.starts_with("[nas.local]:2222 ssh-ed25519 "));
        assert!(written.ends_with('\n'));
        assert_eq!(
            verdict(&file, "nas.local", 2222, &key(KEY_A)),
            Verdict::Known,
            "what was learned is known on the next connection"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn learning_appends_and_does_not_glue_itself_to_an_unterminated_line() {
        let file = fixture("append", &format!("other.local {KEY_B}"));
        learn(&file, "nas.local", 22, &key(KEY_A)).expect("learn");
        let written = std::fs::read_to_string(&file).expect("read back");
        assert_eq!(written.lines().count(), 2);
        assert_eq!(
            verdict(&file, "other.local", 22, &key(KEY_B)),
            Verdict::Known,
            "the line that was already there still parses"
        );
    }

    #[test]
    fn learning_a_key_that_is_already_recorded_writes_nothing() {
        let file = fixture("idempotent", &format!("nas.local {KEY_A}\n"));
        let before = std::fs::read(&file).expect("read");
        learn(&file, "nas.local", 22, &key(KEY_A)).expect("learn");
        assert_eq!(std::fs::read(&file).expect("read"), before);
    }

    /// S6: there is no code path from a changed or a revoked key to a
    /// recorded one. `learn` refuses both itself, so this holds even if a
    /// caller asks it to.
    #[test]
    fn learn_refuses_a_changed_key_and_leaves_the_file_byte_identical() {
        let file = fixture("no-override", &format!("nas.local {KEY_A}\n"));
        let before = std::fs::read(&file).expect("read");
        let refused = learn(&file, "nas.local", 22, &key(KEY_B));
        assert!(refused.is_err(), "a changed key is never recorded");
        assert_eq!(
            std::fs::read(&file).expect("read"),
            before,
            "and the file is not touched"
        );
        assert_eq!(
            verdict(&file, "nas.local", 22, &key(KEY_B)),
            Verdict::Changed {
                line: 1,
                fingerprint: fingerprint(&key(KEY_B))
            },
            "asking again gives the same answer; there is nothing to wear down"
        );
    }

    #[test]
    fn learn_refuses_a_revoked_key_and_leaves_the_file_byte_identical() {
        let file = fixture("no-revoked", &format!("@revoked nas.local {KEY_A}\n"));
        let before = std::fs::read(&file).expect("read");
        assert!(learn(&file, "nas.local", 22, &key(KEY_A)).is_err());
        assert_eq!(std::fs::read(&file).expect("read"), before);
    }

    #[test]
    fn the_unknown_prompt_shows_the_fingerprint_and_the_host() {
        let lines = unknown_lines(&target(2222), FINGERPRINT_A);
        let text = lines.join("\n");
        assert!(text.contains("sftp://thorin@nas.local:2222"));
        assert!(text.contains(FINGERPRINT_A));
    }

    #[test]
    fn the_changed_message_names_the_file_the_line_and_both_fingerprints() {
        let file = fixture("message", &format!("nas.local {KEY_A}\n"));
        let outcome = verdict(&file, "nas.local", 22, &key(KEY_B));
        let text = changed_lines(&target(22), &outcome, &file).join("\n");
        assert!(text.contains("HAS CHANGED"));
        assert!(text.contains(&file.display().to_string()));
        assert!(text.contains("line 1"));
        assert!(
            text.contains(&fingerprint(&key(KEY_B))),
            "the key the server offered now"
        );
        assert!(text.contains(FINGERPRINT_A), "and the one on the line");
        assert!(text.contains("no override"));
        for word in ["Accept", "accept", "continue anyway", "y/n"] {
            assert!(
                !text.contains(word),
                "the refusal must not read as an offer: {word}"
            );
        }
    }

    #[test]
    fn the_revoked_message_offers_nothing_either() {
        let file = fixture("message-revoked", &format!("@revoked nas.local {KEY_A}\n"));
        let outcome = verdict(&file, "nas.local", 22, &key(KEY_A));
        let text = changed_lines(&target(22), &outcome, &file).join("\n");
        assert!(text.contains("REVOKED"));
        assert!(text.contains("no override"));
    }

    #[test]
    fn a_verdict_that_is_not_a_refusal_has_no_refusal_text() {
        let file = fixture("no-message", &format!("nas.local {KEY_A}\n"));
        assert!(changed_lines(&target(22), &Verdict::Known, &file).is_empty());
        assert!(
            changed_lines(
                &target(22),
                &Verdict::Unknown {
                    fingerprint: FINGERPRINT_A.to_string()
                },
                &file
            )
            .is_empty()
        );
    }

    #[test]
    fn glob_matches_the_way_ssh_does() {
        assert!(glob("*.local", "nas.local"));
        assert!(glob("nas.*", "nas.local"));
        assert!(glob("nas?local", "nas.local"));
        assert!(glob("*", "anything"));
        assert!(
            glob("nas.local", "NAS.LOCAL"),
            "hostnames are not case sensitive"
        );
        assert!(!glob("*.local", "local"));
        assert!(!glob("nas?local", "nas..local"));
        assert!(!glob("nas.local", "nas.local.example.org"));
        assert!(glob("[*.local]:2222", "[nas.local]:2222"));
    }
}
