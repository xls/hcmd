//! Dialling an SMB server, and the authentication order on the way in.
//!
//! # Identity
//!
//! An SMB identity has a domain or workgroup in it, and this program spells it
//! **`DOMAIN\user`**. That form and not `user@domain`, for one mechanical
//! reason: the connect line already reads `@` as "the host follows", so
//! `smb://thorin@corp@nas.local` would be ambiguous where
//! `smb://CORP\thorin@nas.local` is not. A line with no backslash has an empty
//! domain, which is what a local account on a NAS box wants.
//!
//! # Anonymous and guest
//!
//! Home NAS boxes ship shares that anyone can read, and smb2 asks for one by
//! sending an empty user and an empty password. A user of `guest`,
//! `anonymous`, or nothing at all is such a login, is dialled with no
//! credential, and is **never prompted**: a prompt everybody dismisses trains
//! people to dismiss prompts. `guest` is sent as the user name where it was
//! typed, because it is a real account on most NAS boxes and some servers want
//! the name with no password; `anonymous` and an empty name are both sent as
//! an empty user, which is what a null session is on the wire.

use std::sync::Arc;

use smb2::{ClientConfig, SmbClient};

use crate::config::RemoteConfig;
use crate::error::{Error, Result};
use crate::remote::auth::{AuthPlan, AuthSequence, MAX_ASKS, Method, Outcome, SecretKind};
use crate::remote::keyring::SecretStore;
use crate::remote::secret::Secret;
use crate::remote::{Protocol, Target};

use super::actor::SmbActor;
use super::{SmbFs, ops};
use crate::remote::prompter::Prompter;

/// Whether a user name means a login with no credential.
///
/// Empty, `guest` and `anonymous`, case-insensitively. The domain half of a
/// `DOMAIN\guest` is ignored, because a guest session has no domain to be in.
pub fn is_guest(user: &str) -> bool {
    let name = split_identity(user).1;
    name.is_empty() || name.eq_ignore_ascii_case("guest") || name.eq_ignore_ascii_case("anonymous")
}

/// `CORP\thorin` split into its domain and its name.
///
/// The **last** backslash, so a name is never mistaken for a domain; a line
/// with no backslash has an empty domain.
pub fn split_identity(user: &str) -> (&str, &str) {
    match user.rfind('\\') {
        Some(at) => (
            user.get(..at).unwrap_or(""),
            user.get(at.saturating_add(1)..).unwrap_or(""),
        ),
        None => ("", user),
    }
}

/// The directory a panel opens on.
///
/// A line that named no share opens on the server, where the shares are - the
/// SMB answer to "wherever the server puts us".
pub fn start_dir(dir: Option<&str>) -> String {
    let Some(dir) = dir else {
        return "/".to_string();
    };
    let trimmed = dir.trim().trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "/" {
        return "/".to_string();
    }
    format!("/{}", trimmed.trim_start_matches('/'))
}

/// One dial, negotiate and session setup.
///
/// The password is a `&str` local to this call and reaches nothing else here.
/// smb2 keeps its own copy inside the client so it can revive a dead session
/// without asking again; that copy is not reachable from anything this program
/// renders.
async fn dial(
    target: &Target,
    password: &str,
    config: &RemoteConfig,
) -> std::result::Result<SmbClient, smb2::Error> {
    let (domain, user) = split_identity(&target.user);
    // `anonymous` is this program's spelling for a null session, and a null
    // session is an **empty** user rather than an account called anonymous.
    // `guest` is a real account name on most NAS boxes, so it is sent as
    // typed, with no password.
    let user = if user.eq_ignore_ascii_case("anonymous") {
        ""
    } else {
        user
    };
    // `target.host` and not `hostname()`: an IPv6 literal keeps its brackets
    // here, which is the form `host:port` has to be written in for a resolver
    // to read the last colon as the port.
    let addr = format!("{}:{}", target.host, target.port);
    SmbClient::connect(ClientConfig {
        addr,
        timeout: config.connect_timeout.duration(),
        username: user.to_string(),
        password: password.to_string(),
        domain: domain.to_string(),
        // A NAS that reboots or a laptop that roams drops the session without
        // dropping the socket. smb2 revives it in place under every handle,
        // and replays only the operations whose retry cannot change what was
        // asked for.
        auto_reconnect: true,
        compression: true,
        dfs_enabled: true,
        dfs_target_overrides: std::collections::HashMap::new(),
    })
    .await
}

/// Whether a failure means "that login will not work" rather than "the server
/// is unreachable", which is the difference between asking again and stopping.
fn rejected(err: &smb2::Error) -> bool {
    matches!(
        err.kind(),
        smb2::ErrorKind::AuthRequired | smb2::ErrorKind::SigningRequired
    )
}

/// Connect, authenticate, and hand back the backend a panel will use.
///
/// The order is fixed and is the order of the code: resolve the identity,
/// dial with each method the plan offers until one is accepted, then wrap the
/// authenticated client in its actor.
pub async fn connect(
    target: Target,
    plan: AuthPlan,
    typed: Option<Secret>,
    config: &RemoteConfig,
    store: Arc<dyn SecretStore>,
    hooks: Prompter,
) -> Result<Arc<SmbFs>> {
    let authority = target.authority();
    let start = start_dir(target.dir.as_deref());

    if is_guest(&target.user) {
        let client = dial(&target, "", config)
            .await
            .map_err(|err| ops::translate(&err, &authority, &authority))?;
        return Ok(finish(target, client, authority, start));
    }

    // The plan is the record of the per-host opt-in: `Stored` is in it only
    // for a host whose `auth` is `keyring`, which is exactly when the prompt
    // may offer to remember the answer.
    let opted_in = plan.methods().iter().any(|m| matches!(m, Method::Stored));
    let mut typed = typed;
    let mut sequence = AuthSequence::new(plan);
    while let Some(method) = sequence.peek().cloned() {
        let (secret, remember) = match method {
            // SMB has neither, and the plan a saved SFTP host would produce is
            // not this host's. Recorded rather than skipped, so the failure
            // message says what was not attempted and why.
            Method::Agent => {
                sequence.record(Outcome::Unavailable(
                    "SMB has no agent authentication".to_string(),
                ));
                continue;
            }
            Method::Key { .. } => {
                sequence.record(Outcome::Unavailable(
                    "SMB has no key authentication".to_string(),
                ));
                continue;
            }
            Method::Stored => match store.get(&target.keyring_account()) {
                Ok(Some(secret)) => (secret, false),
                Ok(None) => {
                    sequence.record(Outcome::Unavailable(
                        "there is no password in the keyring for this host".to_string(),
                    ));
                    continue;
                }
                Err(err) => {
                    sequence.record(Outcome::Unavailable(err.to_string()));
                    continue;
                }
            },
            Method::Password => match typed.take() {
                // A password typed on the connect line is used for this
                // connection and nothing else.
                Some(secret) => (secret, false),
                None => {
                    if sequence.asks() >= MAX_ASKS {
                        sequence
                            .record(Outcome::Unavailable("no password was accepted".to_string()));
                        continue;
                    }
                    let kind = SecretKind::Password {
                        authority: authority.clone(),
                    };
                    match hooks.ask_secret(kind, opted_in && store.available()).await {
                        Some(answer) => (answer.secret, answer.remember),
                        None => {
                            return Err(Error::msg(format!(
                                "{authority}: the connection was cancelled"
                            )));
                        }
                    }
                }
            },
        };

        let Some(password) = secret.expose_str() else {
            sequence.record(Outcome::Unavailable(
                "the password is not valid UTF-8".to_string(),
            ));
            continue;
        };
        match dial(&target, password, config).await {
            Ok(client) => {
                if remember {
                    // Only ever after the server has accepted it, so a typo is
                    // not what gets stored.
                    let _ = store.set(&target.keyring_account(), &secret);
                }
                sequence.record(Outcome::Accepted);
                return Ok(finish(target, client, authority, start));
            }
            Err(err) if rejected(&err) => {
                // Ask again on the same method: a wrong password is
                // re-askable and must not fall through to the next method.
                if matches!(method, Method::Password) && sequence.asks() < MAX_ASKS {
                    sequence.record(Outcome::Needs(SecretKind::Password {
                        authority: authority.clone(),
                    }));
                } else {
                    sequence.record(Outcome::Rejected);
                }
            }
            // Not a rejection: the server could not be reached at all, and
            // trying the next password against a host that is not there would
            // waste the user's time and say nothing.
            Err(err) => return Err(ops::translate(&err, &authority, &authority)),
        }
    }
    Err(Error::msg(sequence.failure_message(&target)))
}

/// Wrap an authenticated client in its actor and its backend.
fn finish(target: Target, client: SmbClient, authority: String, start: String) -> Arc<SmbFs> {
    let ops = SmbActor::start(client, authority);
    SmbFs::new(
        Target {
            protocol: Protocol::Smb,
            ..target
        },
        ops,
        start,
    )
}

#[cfg(test)]
#[path = "connect_tests.rs"]
mod tests;
