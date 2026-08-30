//! Where a stored password lives.
//!
//! > Stored passwords go in the **system keyring** via the `keyring` crate
//! > (Secret Service, kwallet). A password must never be written to
//! > `hosts.toml` or any other file in `~/.config`. If no keyring is
//! > available, say so in the dialog and fall back to prompting every time -
//! > do not silently write the password to disk.
//!
//! There is therefore no fallback file, no cache and no "remember for this
//! session" anywhere in this module. [`NoKeyring`] is what "no keyring
//! available" is made of, and the only thing it can do with a password is
//! refuse it: a type that cannot write is a stronger promise than a branch that
//! chooses not to.
//!
//! # The one copy this module cannot control
//!
//! The `keyring` crate's API hands a stored password back as a `String`
//! (`Entry::get_password`). That `String` is a copy of the secret which
//! `crate::remote::secret::Secret` cannot zero, because it is freed by code in
//! another crate. [`SystemKeyring::get`] copies it into a [`Secret`] and drops
//! it immediately; the window is a few instructions wide and it cannot be
//! closed without a different keyring API. Recorded here rather than left for a
//! reader to notice.
//!
//! # Nothing in `cargo test` touches a real keyring
//!
//! [`store`] is the only function that reaches the Secret Service, and no test
//! calls it: every test in this milestone runs against [`MemoryStore`], which
//! is `#[cfg(test)]` and lives in this file.

use std::sync::{Arc, OnceLock};

use super::secret::Secret;
use crate::error::{Error, Result};

/// Where a stored password lives.
///
/// A trait, so that every test in this milestone runs against an in-memory
/// implementation and the real Secret Service is never touched by `cargo test`.
pub trait SecretStore: Send + Sync {
    /// Whether a keyring is available at all. `false` is a supported state:
    /// the design says to "say so in the dialog and fall back to prompting
    /// every time - do not silently write the password to disk".
    fn available(&self) -> bool;
    /// The password stored under `account`, or `None` when there is none.
    ///
    /// "No entry" is `Ok(None)` and not an error: a host that opted in but has
    /// not saved a password yet is an ordinary state, and the prompt that
    /// follows is not a failure to report.
    fn get(&self, account: &str) -> Result<Option<Secret>>;
    /// Store a password under `account`.
    ///
    /// Called only after the server has accepted it and only where the user
    /// asked for it, so a typo is never stored.
    fn set(&self, account: &str, secret: &Secret) -> Result<()>;
    /// Forget the password stored under `account`. Deleting one that is not
    /// there is not an error.
    fn delete(&self, account: &str) -> Result<()>;
}

/// The service name every entry is filed under.
pub const SERVICE: &str = "holoscommander";

/// The `keyring` crate (Secret Service, kwallet).
///
/// Holds nothing: an entry is built for each call from the service name and the
/// account, so there is no handle for a secret to sit in.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemKeyring;

impl SecretStore for SystemKeyring {
    fn available(&self) -> bool {
        // Initialises the platform store on the first call and remembers the
        // result, which is why `store` can probe once and keep the answer.
        ::keyring::Entry::store_status().is_ok()
    }

    fn get(&self, account: &str) -> Result<Option<Secret>> {
        let entry = ::keyring::Entry::new(SERVICE, account).map_err(describe)?;
        match entry.get_password() {
            Ok(password) => Ok(Some(Secret::from_str(&password))),
            Err(::keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(describe(err)),
        }
    }

    fn set(&self, account: &str, secret: &Secret) -> Result<()> {
        let entry = ::keyring::Entry::new(SERVICE, account).map_err(describe)?;
        let Some(text) = secret.expose_str() else {
            return Err(Error::msg(
                "this password is not text and cannot be stored in the keyring",
            ));
        };
        entry.set_password(text).map_err(describe)
    }

    fn delete(&self, account: &str) -> Result<()> {
        let entry = ::keyring::Entry::new(SERVICE, account).map_err(describe)?;
        match entry.delete_credential() {
            Ok(()) | Err(::keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(describe(err)),
        }
    }
}

/// A `keyring` error as a sentence, without ever formatting it with `Debug`.
///
/// `keyring::Error::BadEncoding` carries the bytes it could not decode, and
/// those bytes are the password. Its `Display` does not print them and its
/// derived `Debug` does, so this function exists to make sure the `Debug` is
/// never the one that gets called (no secret in a log line, an
/// error message or a `Debug` impl).
fn describe(err: ::keyring::Error) -> Error {
    match err {
        ::keyring::Error::NoDefaultStore => Error::msg(unavailable_message()),
        ::keyring::Error::BadEncoding(_) => {
            Error::msg("the stored password is not text; it was left alone")
        }
        other => Error::msg(format!("the system keyring: {other}")),
    }
}

/// Used when there is no keyring: [`NoKeyring::available`] is false,
/// [`NoKeyring::get`] is `Ok(None)`, and [`NoKeyring::set`] is an error naming
/// what would have to be running. It never writes anything anywhere - that is
/// the whole point of the type.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoKeyring;

impl SecretStore for NoKeyring {
    fn available(&self) -> bool {
        false
    }

    fn get(&self, _account: &str) -> Result<Option<Secret>> {
        Ok(None)
    }

    fn set(&self, _account: &str, _secret: &Secret) -> Result<()> {
        Err(Error::msg(unavailable_message()))
    }

    fn delete(&self, _account: &str) -> Result<()> {
        // There is nothing stored, so there is nothing to delete, and saying so
        // as an error would put a failure on the screen for a host being
        // tidied up.
        Ok(())
    }
}

/// The store this session uses. Probes once; a probe failure is [`NoKeyring`]
/// and a warning, never a hard failure.
///
/// Not called by any test: it is the one function here that reaches the Secret
/// Service.
pub fn store() -> Arc<dyn SecretStore> {
    /// One probe per process. The `keyring` crate caches its own
    /// initialisation too, so this is about not asking twice rather than about
    /// the cost of asking.
    static STORE: OnceLock<Arc<dyn SecretStore>> = OnceLock::new();
    Arc::clone(STORE.get_or_init(|| {
        if SystemKeyring.available() {
            Arc::new(SystemKeyring)
        } else {
            Arc::new(NoKeyring)
        }
    }))
}

/// The message the design asks the dialog to show when there is no keyring.
///
/// It says what will happen instead, because a message that only says no
/// teaches nothing: the password still works, it is just asked for every time.
pub fn unavailable_message() -> String {
    "No system keyring is available here, so this password cannot be saved and will be asked \
     for every time."
        .to_string()
}

/// An in-memory [`SecretStore`] for tests, so that `cargo test` never reaches
/// the Secret Service.
///
/// It is deliberately in this file and not in a test module of its own: every
/// area of this milestone that needs a store in a test needs this one, and a
/// second in-memory store would be a second set of semantics.
#[cfg(test)]
pub struct MemoryStore {
    /// What [`SecretStore::available`] answers, so the no-keyring path is
    /// testable without uninstalling anything.
    available: bool,
    /// The entries, by account.
    entries: std::sync::Mutex<std::collections::HashMap<String, Secret>>,
}

#[cfg(test)]
impl MemoryStore {
    /// A store that behaves as a working keyring.
    pub fn new() -> Self {
        Self {
            available: true,
            entries: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// A store that behaves as a session with no keyring in it: the same
    /// refusals [`NoKeyring`] makes, so a test can drive the dialog path
    /// without a second type.
    pub fn unavailable() -> Self {
        Self {
            available: false,
            entries: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// The map, recovering a poisoned lock rather than panicking: a test that
    /// panicked once should fail on that, not on every later assertion.
    fn locked(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, Secret>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// How many entries it holds, for a test that asserts nothing was written.
    pub fn len(&self) -> usize {
        self.locked().len()
    }

    /// Whether it holds nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl SecretStore for MemoryStore {
    fn available(&self) -> bool {
        self.available
    }

    fn get(&self, account: &str) -> Result<Option<Secret>> {
        if !self.available {
            return Ok(None);
        }
        Ok(self.locked().get(account).cloned())
    }

    fn set(&self, account: &str, secret: &Secret) -> Result<()> {
        if !self.available {
            return Err(Error::msg(unavailable_message()));
        }
        self.locked().insert(account.to_string(), secret.clone());
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<()> {
        self.locked().remove(account);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S9, first half: with no keyring, nothing is written anywhere and the
    /// refusal says why.
    #[test]
    fn no_keyring_writes_nothing_and_says_so() {
        let store = NoKeyring;
        assert!(!store.available());
        assert!(
            store
                .get("sftp://thorin@nas.local:2222")
                .expect("get")
                .is_none()
        );
        let refused = store.set("sftp://thorin@nas.local:2222", &Secret::from_str("hunter2"));
        let message = match refused {
            Ok(()) => panic!("a password was accepted by the store that cannot store one"),
            Err(err) => err.to_string(),
        };
        assert!(message.contains("asked for every time"), "{message}");
        assert!(
            !message.contains("hunter2"),
            "the refusal quotes the password"
        );
        // And deleting what was never stored is not an error.
        store
            .delete("sftp://thorin@nas.local:2222")
            .expect("delete");
    }

    /// S9, second half: the same refusal through the trait object the dialog
    /// holds, and the store is still empty afterwards.
    #[test]
    fn an_unavailable_store_refuses_through_the_trait_object() {
        let memory = MemoryStore::unavailable();
        {
            let store: &dyn SecretStore = &memory;
            assert!(!store.available());
            assert!(store.get("account").expect("get").is_none());
            assert!(
                store.set("account", &Secret::from_str("hunter2")).is_err(),
                "a store that cannot store must not pretend to have stored"
            );
        }
        assert!(memory.is_empty(), "nothing was written");
    }

    #[test]
    fn the_memory_store_round_trips_a_password() {
        let store = MemoryStore::new();
        assert!(store.available());
        let account = "sftp://thorin@nas.local:2222";
        assert!(store.get(account).expect("get").is_none());
        store
            .set(account, &Secret::from_str("hunter2"))
            .expect("set");
        assert_eq!(
            store.get(account).expect("get"),
            Some(Secret::from_str("hunter2"))
        );
        assert_eq!(store.len(), 1);
        store.delete(account).expect("delete");
        assert!(store.get(account).expect("get").is_none());
        assert!(store.is_empty());
    }

    #[test]
    fn a_stored_password_is_filed_under_the_authority_and_not_the_label() {
        // Two hosts that differ only by port are two entries: moving a host to
        // another port must not silently pick up the old password.
        let store = MemoryStore::new();
        store
            .set("sftp://thorin@nas.local:22", &Secret::from_str("one"))
            .expect("set");
        store
            .set("sftp://thorin@nas.local:2222", &Secret::from_str("two"))
            .expect("set");
        assert_eq!(
            store.get("sftp://thorin@nas.local:22").expect("get"),
            Some(Secret::from_str("one"))
        );
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn the_unavailable_message_says_what_happens_instead() {
        let message = unavailable_message();
        assert!(message.contains("keyring"));
        assert!(message.contains("asked for every time"));
    }

    #[test]
    fn the_service_name_is_the_package_name() {
        assert_eq!(SERVICE, "holoscommander");
    }
}
