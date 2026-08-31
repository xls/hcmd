//! the four authentication methods, as a state machine.
//!
//! > In preference order, and this order matters:
//! > 1. **SSH agent** (`SSH_AUTH_SOCK`). Try first, always.
//! > 2. **Key file**, path per host, with a passphrase prompt if the key is
//! >    encrypted. Default to the usual `~/.ssh/id_*` candidates.
//! > 3. **Password**, prompted at connect time and held only for the session.
//! > 4. **Stored password**, and only if the user explicitly opts in per host.
//!
//! Nothing in this file performs I/O beyond asking whether a candidate key file
//! exists, and nothing in it holds a secret: an [`AuthPlan`] is a list of
//! *methods*, and the password that satisfies one lives in the connect task.
//! That is what makes the order testable with no server, no keyring and no
//! agent, which is the whole of the second bullet.
//!
//! # Why the plan is built per host
//!
//! Taken as a sequence of attempts every host makes, the list can never reach
//! method 4: a prompt always produces a password, so method 3 always succeeds
//! at producing something and method 4 is dead code. It is a preference order
//! over the methods *a host offers*, which is what the design resolves: a
//! host that opted in (`auth = "keyring"`) gets [`Method::Stored`] before
//! [`Method::Password`], because for that host the keyring is where method
//! 3's password comes from and the prompt is what happens when the lookup
//! misses. A host that did not opt in never has [`Method::Stored`] in its
//! plan at all. The enum's declaration order is the; the per-host plan is
//! what varies.

use std::path::{Path, PathBuf};

use super::Target;
use super::hosts::{AuthMethod, SavedHost, expand_tilde};
use super::url::Parsed;

/// One way to authenticate, in the order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    /// 1. SSH agent (`SSH_AUTH_SOCK`). Tried first, always.
    Agent,
    /// 2. A key file, with a passphrase prompt when it is encrypted.
    Key {
        /// The key file, `~` already expanded, so a message names a real path.
        path: PathBuf,
    },
    /// 3. A password, prompted at connect time and held only for the session.
    Password,
    /// 4. A stored password, and only where the user opted in per host.
    Stored,
}

impl Method {
    /// `"agent"`, `"key"`, `"password"`, `"keyring"` - the `hosts.toml`
    /// vocabulary, so a message and a file say the same word.
    pub fn id(&self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Key { .. } => "key",
            Self::Password => "password",
            Self::Stored => "keyring",
        }
    }

    /// What the status line calls it: "the SSH agent", "the key
    /// ~/.ssh/id_ed25519", "a password", "the stored password".
    pub fn describe(&self) -> String {
        match self {
            Self::Agent => "the SSH agent".to_string(),
            Self::Key { path } => format!("the key {}", shorten(path)),
            Self::Password => "a password".to_string(),
            Self::Stored => "the stored password".to_string(),
        }
    }
}

/// `$HOME/.ssh/id_ed25519` written back as `~/.ssh/id_ed25519`.
///
/// Reads the environment, never the filesystem: a message that says `~` is the
/// one the user typed into `hosts.toml`, and the expanded form is noise in a
/// status line.
fn shorten(path: &Path) -> String {
    let rendered = path.display().to_string();
    let Ok(home) = crate::config::paths::home_dir() else {
        return rendered;
    };
    let home = home.display().to_string();
    if home.is_empty() {
        return rendered;
    }
    match rendered.strip_prefix(&home) {
        Some(rest) => format!("~{rest}"),
        None => rendered,
    }
}

/// The candidate key files, in the order the "usual `~/.ssh/id_*`
/// candidates" implies: ed25519, ecdsa, rsa.
///
/// Only files that exist are included, so a plan never has an attempt that
/// cannot be made. This is the one place in this module that touches the
/// filesystem, and it reads no file: it asks whether a path is there.
pub fn default_keys(home: &Path) -> Vec<PathBuf> {
    let ssh = home.join(".ssh");
    ["id_ed25519", "id_ecdsa", "id_rsa"]
        .iter()
        .map(|name| ssh.join(name))
        .filter(|path| path.exists())
        .collect()
}

/// What a connection will try, in order.
///
/// Never holds a secret. A password typed on a quick-connect line is carried by
/// `crate::remote::url::Parsed` and handed to the [`Method::Password`] attempt
/// by the connect task; putting it here would put it in every `Debug` of every
/// plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthPlan {
    /// the order, filtered to what this host offers.
    methods: Vec<Method>,
}

impl AuthPlan {
    /// A saved host's plan, from its `auth` field
    /// (the table).
    ///
    /// A host whose protocol does not verify a host key is an FTP or an SMB
    /// host, and it gets [`AuthPlan::for_password_login`]'s plan instead:
    /// neither protocol has an agent or a key, and offering it either would be
    /// an attempt that cannot be made.
    pub fn for_host(host: &SavedHost, home: &Path) -> Self {
        if !host.protocol.verifies_host_key() {
            return Self::for_password_login(Some(host));
        }
        let mut methods = vec![Method::Agent];
        match host.auth {
            // `agent` and an absent `auth` are the same plan: the agent is
            // tried first always, so naming it adds a preference for the
            // agent and takes nothing away from the keys behind it.
            AuthMethod::Agent => {
                methods.extend(
                    default_keys(home)
                        .into_iter()
                        .map(|path| Method::Key { path }),
                );
                methods.push(Method::Password);
            }
            AuthMethod::Key => {
                if host.key_file.trim().is_empty() {
                    // `auth = "key"` with no `key_file` means the defaults,
                    // which is what the "default to the usual
                    // ~/.ssh/id_* candidates" says to do.
                    methods.extend(
                        default_keys(home)
                            .into_iter()
                            .map(|path| Method::Key { path }),
                    );
                } else {
                    // A configured key is kept even when it is not there, so
                    // the failure message names the file the user asked for
                    // rather than silently trying something else.
                    methods.push(Method::Key {
                        path: expand_tilde(&host.key_file, home),
                    });
                }
                methods.push(Method::Password);
            }
            AuthMethod::Password => methods.push(Method::Password),
            AuthMethod::Keyring => {
                methods.push(Method::Stored);
                methods.push(Method::Password);
            }
        }
        Self { methods }
    }

    /// A quick-connect line's plan: agent, then each default key, then a
    /// prompted password. Never [`Method::Stored`], because opting in is per
    /// host and a typed line is not a host.
    ///
    /// An FTP line gets FTP's plan, and an anonymous FTP line gets none.
    ///
    pub fn for_line(parsed: &Parsed, home: &Path) -> Self {
        if !parsed.target.protocol.verifies_host_key() {
            return Self {
                methods: password_methods(parsed.target.protocol, &parsed.target.user, false),
            };
        }
        let mut methods = vec![Method::Agent];
        methods.extend(
            default_keys(home)
                .into_iter()
                .map(|path| Method::Key { path }),
        );
        methods.push(Method::Password);
        Self { methods }
    }

    /// The plan for a protocol whose only method is a password - FTP, FTPS
    /// and SMB. Exactly one entry, [`Method::Password`], or [`Method::Stored`]
    /// then [`Method::Password`] for a host that opted in. A login that
    /// carries no credential at all - anonymous FTP, guest SMB - has none.
    ///
    /// `None` is a quick-connect line whose user needs a credential;
    /// [`AuthPlan::for_line`] answers the credential-free case itself, because
    /// a line carries the protocol and the user and this argument does not.
    pub fn for_password_login(host: Option<&SavedHost>) -> Self {
        let methods = match host {
            Some(host) => password_methods(
                host.protocol,
                &host.username,
                host.auth == AuthMethod::Keyring,
            ),
            None => vec![Method::Password],
        };
        Self { methods }
    }

    /// The methods, in the order.
    pub fn methods(&self) -> &[Method] {
        &self.methods
    }
}

/// The rule in one place, for both protocols that have only a password:
/// a login that carries no credential is never prompted, so its plan is empty;
/// anything else is a password, preceded by the stored one when the user opted
/// in.
fn password_methods(protocol: super::Protocol, user: &str, stored: bool) -> Vec<Method> {
    if is_credential_free(protocol, user) {
        return Vec::new();
    }
    let mut methods = Vec::new();
    if stored {
        methods.push(Method::Stored);
    }
    methods.push(Method::Password);
    methods
}

/// Whether a login carries no credential at all, per protocol.
///
/// FTP's is `anonymous`, which logs in with [`ANONYMOUS_PASSWORD`]; SMB's is
/// a guest or null session, which logs in with nothing. The SSH family has no
/// such login: there is always a key or a password.
pub fn is_credential_free(protocol: super::Protocol, user: &str) -> bool {
    match protocol {
        super::Protocol::Sftp => false,
        super::Protocol::Ftp | super::Protocol::Ftps | super::Protocol::FtpsImplicit => {
            is_anonymous(user)
        }
        super::Protocol::Smb => super::smb::connect::is_guest(user),
        // A WebDAV server may well be open, and an empty user is how somebody
        // says so. The request is made without an `Authorization` header and
        // the server answers 401 if it wanted one, which is a better question
        // to ask the network than to ask the user in advance.
        super::Protocol::Dav | super::Protocol::Davs => user.is_empty(),
    }
}

/// Whether an FTP user name means an anonymous login (the design's
/// `ftp://anonymous@ftp.example.org`).
pub fn is_anonymous(user: &str) -> bool {
    let user = user.trim();
    user.is_empty() || user.eq_ignore_ascii_case("anonymous")
}

/// The password an anonymous FTP login sends.
///
/// Not a secret: it is a convention, it is the same for everybody, and it is
/// deliberately not an email address. Nobody types a real one, and a prompt
/// that everybody dismisses trains people to dismiss prompts.
pub const ANONYMOUS_PASSWORD: &str = "anonymous@";

/// How an attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The server accepted it.
    Accepted,
    /// The server rejected it. Try the next method.
    Rejected,
    /// It could not be attempted at all: no `SSH_AUTH_SOCK`, no such key file,
    /// no keyring. The reason is shown once at the end, never as a failure.
    ///
    /// The string is written by this program and never by a server: a server's
    /// rejection can quote what was sent to it, and this string ends up on the
    /// screen (see [`AuthSequence::failure_message`]).
    Unavailable(String),
    /// It needs a secret from the user before it can be attempted.
    Needs(SecretKind),
}

/// Which question to put on the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretKind {
    /// "Passphrase for ~/.ssh/id_ed25519:"
    Passphrase {
        /// The key file that will not decrypt without it.
        key: PathBuf,
    },
    /// "Password for thorin@nas.local:"
    Password {
        /// [`Target::authority`], which carries no secret.
        authority: String,
    },
}

impl SecretKind {
    /// The dialog's title, which never contains a secret because neither
    /// variant carries one.
    pub fn title(&self) -> String {
        match self {
            Self::Passphrase { key } => format!("Passphrase for {}", shorten(key)),
            Self::Password { authority } => format!("Password for {authority}"),
        }
    }

    /// "the passphrase" or "the password", for a sentence about one.
    fn what(&self) -> &'static str {
        match self {
            Self::Passphrase { .. } => "the passphrase",
            Self::Password { .. } => "the password",
        }
    }
}

/// the order, as a state machine, so it is testable with no
/// transport at all.
///
/// It is deliberately ignorant of how a method is attempted: it knows which one
/// is next, what happened to the last one, and how many times the user has been
/// asked for something. A caller that never calls [`AuthSequence::record`]
/// never advances, which is what makes S7 - verification before any attempt -
/// checkable by inspection.
#[derive(Debug)]
pub struct AuthSequence {
    /// What this host offers, in order.
    plan: AuthPlan,
    /// Where in the plan the next attempt is.
    at: usize,
    /// Every finished attempt and how it ended, for the failure message.
    tried: Vec<(Method, Outcome)>,
    /// Set once a method is accepted; nothing is attempted afterwards.
    accepted: bool,
    /// How many secrets have been asked for, against [`MAX_ASKS`].
    asks: usize,
}

impl AuthSequence {
    /// A sequence that has attempted nothing.
    pub fn new(plan: AuthPlan) -> Self {
        Self {
            plan,
            at: 0,
            tried: Vec::new(),
            accepted: false,
            asks: 0,
        }
    }

    /// The next method to attempt, or `None` when the plan is exhausted or one
    /// of them has already been accepted. Does not advance;
    /// [`AuthSequence::record`] does.
    pub fn peek(&self) -> Option<&Method> {
        if self.accepted {
            return None;
        }
        self.plan.methods().get(self.at)
    }

    /// Record how the current attempt ended and advance.
    ///
    /// [`Outcome::Needs`] does **not** advance: the same method is attempted
    /// again once the secret arrives, which is what makes a wrong passphrase
    /// re-askable without falling through to the next method. Past
    /// [`MAX_ASKS`] it does advance, recorded as unavailable, so a server that
    /// answers every secret with another question cannot loop for ever.
    ///
    ///
    /// Recording when the plan is exhausted does nothing.
    pub fn record(&mut self, outcome: Outcome) {
        let Some(method) = self.plan.methods().get(self.at).cloned() else {
            return;
        };
        match outcome {
            Outcome::Needs(kind) => {
                self.asks += 1;
                if self.asks > MAX_ASKS {
                    self.tried.push((
                        method,
                        Outcome::Unavailable(format!(
                            "{} was asked for {MAX_ASKS} times and refused each time",
                            kind.what()
                        )),
                    ));
                    self.at += 1;
                }
            }
            Outcome::Accepted => {
                self.accepted = true;
                self.tried.push((method, Outcome::Accepted));
                self.at += 1;
            }
            Outcome::Rejected => {
                self.tried.push((method, Outcome::Rejected));
                self.at += 1;
            }
            Outcome::Unavailable(why) => {
                self.tried.push((method, Outcome::Unavailable(why)));
                self.at += 1;
            }
        }
    }

    /// Whether a method was accepted.
    pub fn accepted(&self) -> bool {
        self.accepted
    }

    /// What to put on the screen when nothing worked: every method that was
    /// tried and why each one failed.
    ///
    /// Never a secret - no variant of [`Method`] or [`Outcome`] can hold one -
    /// and never the server's raw error, which is why [`Outcome::Unavailable`]
    /// carries a string this program wrote.
    pub fn failure_message(&self, target: &Target) -> String {
        let authority = target.authority();
        if self.plan.methods().is_empty() {
            return format!("{authority}: no authentication method is configured for this host");
        }
        let mut message = format!("{authority}: could not authenticate.");
        for (method, outcome) in &self.tried {
            let reason = match outcome {
                Outcome::Accepted => "accepted".to_string(),
                Outcome::Rejected => "rejected by the server".to_string(),
                Outcome::Unavailable(why) => why.clone(),
                // `record` never stores a `Needs`: it is a question, not an
                // ending. The arm exists because this crate's enums are
                // matched exhaustively.
                Outcome::Needs(kind) => format!("{} was never given", kind.what()),
            };
            message.push_str(&format!("\n  {}: {reason}", method.describe()));
        }
        let untried = self.plan.methods().len().saturating_sub(self.tried.len());
        if untried > 0 {
            message.push_str(&format!(
                "\n  {untried} further method(s) were not attempted"
            ));
        }
        message
    }

    /// How many times a secret has been asked for on this sequence, so a
    /// server that rejects everything cannot loop for ever.
    ///
    pub fn asks(&self) -> usize {
        self.asks
    }
}

/// The most times one connection attempt will ask for a secret before giving
/// up. Three, which is what `ssh` itself allows.
pub const MAX_ASKS: usize = 3;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::Protocol;
    use crate::remote::secret::Secret;

    fn host(auth: AuthMethod, key_file: &str) -> SavedHost {
        SavedHost {
            label: "nas".to_string(),
            protocol: Protocol::Sftp,
            host: "nas.local".to_string(),
            port: 2222,
            username: "thorin".to_string(),
            auth,
            key_file: key_file.to_string(),
            remote_dir: "/srv/media".to_string(),
            local_dir: String::new(),
        }
    }

    fn target() -> Target {
        Target {
            protocol: Protocol::Sftp,
            host: "nas.local".to_string(),
            port: 2222,
            user: "thorin".to_string(),
            dir: None,
        }
    }

    /// A home with `.ssh/id_ed25519` and `.ssh/id_rsa` in it, so
    /// [`default_keys`] has something to find and the order is observable.
    fn home_with_keys() -> PathBuf {
        let home = std::env::temp_dir().join(format!(
            "hcmd-auth-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let ssh = home.join(".ssh");
        std::fs::create_dir_all(&ssh).expect("create the fixture home");
        std::fs::write(ssh.join("id_ed25519"), b"not a key").expect("write id_ed25519");
        std::fs::write(ssh.join("id_rsa"), b"not a key").expect("write id_rsa");
        home
    }

    #[test]
    fn default_keys_are_ed25519_then_ecdsa_then_rsa_and_only_if_they_exist() {
        let home = home_with_keys();
        let keys = default_keys(&home);
        let names: Vec<String> = keys
            .iter()
            .filter_map(|path| path.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        // id_ecdsa is not in the fixture, so it is not in the plan.
        assert_eq!(names, vec!["id_ed25519".to_string(), "id_rsa".to_string()]);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn an_agent_host_tries_the_agent_then_the_keys_then_a_password() {
        let home = home_with_keys();
        let plan = AuthPlan::for_host(&host(AuthMethod::Agent, ""), &home);
        let ids: Vec<&str> = plan.methods().iter().map(Method::id).collect();
        assert_eq!(ids, vec!["agent", "key", "key", "password"]);
        assert_eq!(plan.methods().first(), Some(&Method::Agent));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_key_host_tries_the_agent_then_that_key_then_a_password() {
        let home = home_with_keys();
        let plan = AuthPlan::for_host(&host(AuthMethod::Key, "~/.ssh/id_ed25519"), &home);
        assert_eq!(
            plan.methods(),
            &[
                Method::Agent,
                Method::Key {
                    path: home.join(".ssh").join("id_ed25519")
                },
                Method::Password,
            ]
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_configured_key_that_is_not_there_is_still_in_the_plan() {
        let home = home_with_keys();
        let plan = AuthPlan::for_host(&host(AuthMethod::Key, "~/.ssh/id_missing"), &home);
        assert!(plan.methods().contains(&Method::Key {
            path: home.join(".ssh").join("id_missing")
        }));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_password_host_tries_the_agent_then_a_password_and_never_the_keyring() {
        let home = home_with_keys();
        let plan = AuthPlan::for_host(&host(AuthMethod::Password, ""), &home);
        assert_eq!(plan.methods(), &[Method::Agent, Method::Password]);
        assert!(
            !plan.methods().contains(&Method::Stored),
            "a host that did not opt in is never asked of the keyring"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_keyring_host_looks_in_the_keyring_before_it_prompts() {
        let home = home_with_keys();
        let plan = AuthPlan::for_host(&host(AuthMethod::Keyring, ""), &home);
        assert_eq!(
            plan.methods(),
            &[Method::Agent, Method::Stored, Method::Password],
            "otherwise the keyring can never be reached"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_quick_connect_line_never_has_a_stored_password_in_its_plan() {
        let home = home_with_keys();
        let parsed = Parsed {
            target: target(),
            password: Some(Secret::from_str("hunter2")),
        };
        let plan = AuthPlan::for_line(&parsed, &home);
        assert_eq!(plan.methods().first(), Some(&Method::Agent));
        assert_eq!(plan.methods().last(), Some(&Method::Password));
        assert!(!plan.methods().contains(&Method::Stored));
        // And the plan itself holds no secret, so its `Debug` cannot leak one.
        assert!(!format!("{plan:?}").contains("hunter2"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn ftp_has_one_method_and_an_anonymous_login_has_none() {
        let mut ftp = host(AuthMethod::Password, "");
        ftp.protocol = Protocol::Ftp;
        ftp.username = "thorin".to_string();
        assert_eq!(
            AuthPlan::for_password_login(Some(&ftp)).methods(),
            &[Method::Password]
        );

        ftp.auth = AuthMethod::Keyring;
        assert_eq!(
            AuthPlan::for_password_login(Some(&ftp)).methods(),
            &[Method::Stored, Method::Password]
        );

        ftp.username = "anonymous".to_string();
        assert!(
            AuthPlan::for_password_login(Some(&ftp))
                .methods()
                .is_empty()
        );
        ftp.username = String::new();
        assert!(
            AuthPlan::for_password_login(Some(&ftp))
                .methods()
                .is_empty()
        );
    }

    /// SMB's plan is FTP's shape with SMB's rule for who needs no credential:
    /// a guest or anonymous share is opened, not interrogated.
    #[test]
    fn smb_has_one_method_and_a_guest_login_has_none() {
        let mut smb = host(AuthMethod::Password, "");
        smb.protocol = Protocol::Smb;
        smb.username = "CORP\\thorin".to_string();
        assert_eq!(
            AuthPlan::for_password_login(Some(&smb)).methods(),
            &[Method::Password]
        );

        smb.auth = AuthMethod::Keyring;
        assert_eq!(
            AuthPlan::for_password_login(Some(&smb)).methods(),
            &[Method::Stored, Method::Password]
        );

        for guest in ["guest", "GUEST", "anonymous", "", "WORKGROUP\\guest"] {
            smb.username = guest.to_string();
            assert!(
                AuthPlan::for_password_login(Some(&smb))
                    .methods()
                    .is_empty(),
                "{guest:?} carries no credential"
            );
        }

        // And an SMB host is never offered an agent or a key either.
        let home = std::path::PathBuf::from("/nonexistent");
        smb.username = "thorin".to_string();
        smb.auth = AuthMethod::Agent;
        assert_eq!(
            AuthPlan::for_host(&smb, &home).methods(),
            &[Method::Password]
        );
    }

    #[test]
    fn an_ftp_host_is_never_offered_an_agent_or_a_key() {
        let home = home_with_keys();
        let mut ftp = host(AuthMethod::Agent, "~/.ssh/id_ed25519");
        ftp.protocol = Protocol::Ftps;
        let plan = AuthPlan::for_host(&ftp, &home);
        assert_eq!(plan.methods(), &[Method::Password]);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn an_anonymous_ftp_line_is_never_prompted() {
        let home = home_with_keys();
        let mut anonymous = target();
        anonymous.protocol = Protocol::Ftp;
        anonymous.user = "anonymous".to_string();
        let parsed = Parsed {
            target: anonymous,
            password: None,
        };
        assert!(AuthPlan::for_line(&parsed, &home).methods().is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_sequence_walks_the_plan_in_order() {
        let plan = AuthPlan {
            methods: vec![Method::Agent, Method::Stored, Method::Password],
        };
        let mut sequence = AuthSequence::new(plan);
        assert_eq!(sequence.peek(), Some(&Method::Agent));
        sequence.record(Outcome::Unavailable("SSH_AUTH_SOCK is not set".to_string()));
        assert_eq!(sequence.peek(), Some(&Method::Stored));
        sequence.record(Outcome::Rejected);
        assert_eq!(sequence.peek(), Some(&Method::Password));
        sequence.record(Outcome::Accepted);
        assert_eq!(sequence.peek(), None);
        assert!(sequence.accepted());
    }

    #[test]
    fn an_unavailable_method_is_not_a_failure() {
        let plan = AuthPlan {
            methods: vec![Method::Agent, Method::Password],
        };
        let mut sequence = AuthSequence::new(plan);
        sequence.record(Outcome::Unavailable("SSH_AUTH_SOCK is not set".to_string()));
        sequence.record(Outcome::Accepted);
        assert!(
            sequence.accepted(),
            "the agent being absent is not a refusal"
        );
    }

    #[test]
    fn needing_a_secret_attempts_the_same_method_again() {
        let key = PathBuf::from("/home/thorin/.ssh/id_ed25519");
        let plan = AuthPlan {
            methods: vec![Method::Key { path: key.clone() }, Method::Password],
        };
        let mut sequence = AuthSequence::new(plan);
        assert_eq!(sequence.peek(), Some(&Method::Key { path: key.clone() }));
        sequence.record(Outcome::Needs(SecretKind::Passphrase { key: key.clone() }));
        assert_eq!(
            sequence.peek(),
            Some(&Method::Key { path: key }),
            "a wrong passphrase is asked again, it does not fall through"
        );
        assert_eq!(sequence.asks(), 1);
    }

    #[test]
    fn a_secret_is_asked_for_at_most_three_times() {
        let key = PathBuf::from("/home/thorin/.ssh/id_ed25519");
        let plan = AuthPlan {
            methods: vec![Method::Key { path: key.clone() }, Method::Password],
        };
        let mut sequence = AuthSequence::new(plan);
        for _ in 0..MAX_ASKS {
            sequence.record(Outcome::Needs(SecretKind::Passphrase { key: key.clone() }));
        }
        assert_eq!(sequence.asks(), MAX_ASKS);
        assert_eq!(sequence.peek(), Some(&Method::Key { path: key.clone() }));
        // The fourth question is where it stops asking and moves on.
        sequence.record(Outcome::Needs(SecretKind::Passphrase { key }));
        assert_eq!(sequence.peek(), Some(&Method::Password));
    }

    #[test]
    fn recording_past_the_end_of_the_plan_does_nothing() {
        let mut sequence = AuthSequence::new(AuthPlan {
            methods: vec![Method::Password],
        });
        sequence.record(Outcome::Rejected);
        assert_eq!(sequence.peek(), None);
        sequence.record(Outcome::Rejected);
        sequence.record(Outcome::Accepted);
        assert!(!sequence.accepted(), "there was nothing left to accept");
    }

    #[test]
    fn the_failure_message_names_every_method_and_no_secret() {
        let key = PathBuf::from("/home/thorin/.ssh/id_ed25519");
        let plan = AuthPlan {
            methods: vec![Method::Agent, Method::Key { path: key }, Method::Password],
        };
        let mut sequence = AuthSequence::new(plan);
        sequence.record(Outcome::Unavailable("SSH_AUTH_SOCK is not set".to_string()));
        sequence.record(Outcome::Unavailable(
            "there is no such key file".to_string(),
        ));
        sequence.record(Outcome::Rejected);
        let message = sequence.failure_message(&target());
        assert!(message.contains("sftp://thorin@nas.local:2222"));
        assert!(message.contains("the SSH agent"));
        assert!(message.contains("SSH_AUTH_SOCK is not set"));
        assert!(message.contains("id_ed25519"));
        assert!(message.contains("a password"));
        assert!(message.contains("rejected by the server"));
        assert!(!message.contains("hunter2"));
    }

    #[test]
    fn a_host_with_no_method_says_so_rather_than_failing_silently() {
        let sequence = AuthSequence::new(AuthPlan {
            methods: Vec::new(),
        });
        let message = sequence.failure_message(&target());
        assert!(message.contains("no authentication method is configured"));
    }

    #[test]
    fn the_prompt_titles_carry_no_secret() {
        let passphrase = SecretKind::Passphrase {
            key: PathBuf::from("/home/thorin/.ssh/id_ed25519"),
        };
        assert!(passphrase.title().starts_with("Passphrase for "));
        let password = SecretKind::Password {
            authority: target().authority(),
        };
        assert_eq!(
            password.title(),
            "Password for sftp://thorin@nas.local:2222"
        );
    }

    /// S7, as far as one file can carry it: building a plan and asking what is
    /// next attempts nothing. The connect task verifies the host key before it
    /// makes the first attempt, and this is the
    /// half of that which is testable with no transport: nothing here advances
    /// on its own, so a sequence that was never recorded against has never
    /// authenticated.
    #[test]
    fn a_sequence_attempts_nothing_until_it_is_told_something_happened() {
        let home = home_with_keys();
        let plan = AuthPlan::for_host(&host(AuthMethod::Keyring, ""), &home);
        let sequence = AuthSequence::new(plan);
        assert!(sequence.tried.is_empty());
        assert_eq!(sequence.asks(), 0);
        assert!(!sequence.accepted());
        assert_eq!(
            sequence.peek(),
            Some(&Method::Agent),
            "peeking is not attempting"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn every_method_has_the_hosts_toml_word_for_it() {
        assert_eq!(Method::Agent.id(), "agent");
        assert_eq!(
            Method::Key {
                path: PathBuf::new()
            }
            .id(),
            "key"
        );
        assert_eq!(Method::Password.id(), "password");
        assert_eq!(Method::Stored.id(), "keyring");
    }
}
