//! A password or a passphrase while it is in memory.
//!
//! the design says a password is "prompted at connect time and held only for
//! the session", and that it must never be written to `hosts.toml` or any other
//! file in `~/.config`. Both halves of that are properties of a *type* here
//! rather than habits of the call sites: [`Secret`] has no `Display`, no
//! `serde` impls and no `AsRef<str>`, so the ways it can leave this module are
//! [`Secret::expose`] and [`Secret::expose_str`], which are one `grep` away
//! from a review. the design fixes the shape and the S1 to S5 are the tests.
//!
//! # What this type does not promise
//!
//! Zeroing on drop is best effort. the design argues that reliable zeroing
//! needs `zeroize`; `zeroize` is not in the crate table and is not
//! added here, so [`Drop`] overwrites the buffer
//! in safe Rust and asks the optimiser not to elide the writes. It cannot stop
//! the kernel from having paged the buffer out, and it cannot reach a copy some
//! other crate's API has already made - `keyring` hands out a `String`, and
//! that `String` is beyond this type's reach (see `crate::remote::keyring`).

use std::fmt;

/// A password or a passphrase, in memory, for as long as it is needed.
///
/// Four properties, each of which is a test (S1 to S4 of
/// the design):
///
/// * `Debug` prints `Secret(<redacted>)`. There is no `Display`, no
///   `Serialize`, no `AsRef<str>` and no `into_string`: the only way out is
///   [`Secret::expose`], which is one grep away from a review.
/// * `Drop` overwrites the buffer, byte by byte, in safe Rust. This is best
///   effort and says so in the module documentation.
/// * The buffer is allocated once at [`Secret::MAX`] and never grows, so
///   typing into a prompt cannot leave a reallocated copy behind.
/// * Input past [`Secret::MAX`] is refused rather than reallocating.
pub struct Secret {
    /// The bytes, always valid UTF-8 because the only way in is a `char`.
    /// Allocated once at [`Secret::MAX`] and never grown.
    bytes: Vec<u8>,
}

impl Secret {
    /// Longer than any passphrase and shorter than a paste accident.
    pub const MAX: usize = 1024;

    /// An empty secret with its whole buffer already allocated.
    pub fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(Self::MAX),
        }
    }

    /// A secret from text that is already in memory - a keyring lookup, or a
    /// password parsed out of a quick-connect line.
    ///
    /// Characters past [`Secret::MAX`] are dropped rather than reallocating,
    /// which is the same refusal [`Secret::push`] makes. A truncated password
    /// fails to authenticate, which is the safe direction to fail in.
    //
    // The name is fixed by the design and four agents write against it.
    // `FromStr` is the trait clippy suggests, and its `Result` would be a lie:
    // this cannot fail, it can only refuse the tail of an absurd input. Allowed
    // on the item, never at the crate root.
    #[allow(
        clippy::should_implement_trait,
        reason = "FromStr's Result would be a lie: this cannot fail, it can only \
                  refuse the tail of an absurd input"
    )]
    pub fn from_str(text: &str) -> Self {
        let mut secret = Self::new();
        for ch in text.chars() {
            if !secret.push(ch) {
                break;
            }
        }
        secret
    }

    /// Append one character. `false` when the secret is already at
    /// [`Secret::MAX`], in which case nothing was appended.
    pub fn push(&mut self, ch: char) -> bool {
        let mut buf = [0u8; 4];
        let encoded = ch.encode_utf8(&mut buf);
        if self.bytes.len() + encoded.len() > Self::MAX {
            return false;
        }
        // Cannot reallocate: the capacity is MAX and the length stays under it,
        // which is what keeps a half-typed password from being left in a freed
        // allocation.
        self.bytes.extend_from_slice(encoded.as_bytes());
        true
    }

    /// Remove the last character. `false` when there was none.
    ///
    /// A whole `char`, not a byte: the buffer is UTF-8 and half a character is
    /// not a shorter password, it is a corrupt one.
    pub fn pop(&mut self) -> bool {
        let len = self.bytes.len();
        let mut cut = len;
        while cut > 0 {
            cut -= 1;
            match self.bytes.get(cut) {
                // A UTF-8 continuation byte: keep walking back to the leader.
                Some(byte) if byte & 0b1100_0000 == 0b1000_0000 => continue,
                Some(_) => break,
                None => return false,
            }
        }
        if cut == len {
            return false;
        }
        // Overwrite before truncating: `Vec::truncate` only moves the length.
        for index in cut..len {
            if let Some(byte) = self.bytes.get_mut(index) {
                *byte = 0;
            }
        }
        self.bytes.truncate(cut);
        true
    }

    /// The length in bytes, for a prompt that draws one asterisk per byte.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether nothing has been typed.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The bytes.
    ///
    /// Every call site is a place a secret can escape; the S5 budgets
    /// four in the whole tree and
    /// [`the_only_places_a_secret_escapes_are_in_remote`] counts them.
    pub fn expose(&self) -> &[u8] {
        &self.bytes
    }

    /// The bytes as `&str` when they are UTF-8, which they are whenever they
    /// came from a keyboard. `None` otherwise, and the caller says so rather
    /// than lossily converting a credential.
    ///
    /// A secret built through [`Secret::push`] is always UTF-8; one built from
    /// bytes a store handed back need not be, which is why this is an `Option`
    /// and not an unwrap.
    pub fn expose_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.bytes).ok()
    }
}

impl Default for Secret {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Secret {
    /// S1: the bytes are never in the output, whatever they are, and the
    /// length is not either - a length is a hint about a password.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl Drop for Secret {
    /// Best effort, and the module documentation says why it cannot be more.
    fn drop(&mut self) {
        for byte in self.bytes.iter_mut() {
            *byte = 0;
        }
        // Safe Rust cannot force a volatile write; this is the strongest hint
        // available without `zeroize` or `unsafe`
        // (`src/lib.rs` forbids it).
        let _ = std::hint::black_box(&self.bytes);
    }
}

impl Clone for Secret {
    /// A fresh [`Secret::MAX`] buffer, so a clone cannot reallocate either.
    fn clone(&self) -> Self {
        let mut copy = Self::new();
        copy.bytes.extend_from_slice(&self.bytes);
        copy
    }
}

impl PartialEq for Secret {
    /// Byte equality. Not constant time, and used by tests only: nothing in
    /// this program compares two secrets to decide anything.
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for Secret {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_the_bytes() {
        let secret = Secret::from_str("hunter2");
        let rendered = format!("{secret:?}");
        assert_eq!(rendered, "Secret(<redacted>)");
        assert!(!rendered.contains("hunter2"));
        // An empty one renders the same way: the output says nothing about
        // what is inside, including whether anything is.
        assert_eq!(format!("{:?}", Secret::new()), "Secret(<redacted>)");
    }

    #[test]
    fn debug_of_a_struct_holding_one_redacts_too() {
        // The derived `Debug` of anything containing a `Secret` inherits the
        // redaction, which is what makes `#[derive(Debug)]` safe on
        // `crate::remote::url::Parsed`.
        #[derive(Debug)]
        struct Holder {
            password: Secret,
        }
        let holder = Holder {
            password: Secret::from_str("hunter2"),
        };
        assert_eq!(holder.password.len(), 7);
        assert!(!format!("{holder:?}").contains("hunter2"));
    }

    #[test]
    fn the_buffer_never_grows_and_input_past_the_limit_is_refused() {
        let mut secret = Secret::new();
        let capacity = secret.bytes.capacity();
        assert_eq!(capacity, Secret::MAX);
        for _ in 0..Secret::MAX {
            assert!(secret.push('x'));
        }
        assert_eq!(secret.len(), Secret::MAX);
        assert!(!secret.push('x'), "the limit refuses rather than growing");
        assert_eq!(
            secret.bytes.capacity(),
            capacity,
            "a reallocation would leave a copy of the password behind"
        );
    }

    #[test]
    fn a_multibyte_character_that_does_not_fit_is_refused_whole() {
        let mut secret = Secret::new();
        for _ in 0..Secret::MAX - 1 {
            assert!(secret.push('x'));
        }
        // Two bytes, one free: refused, and the one free byte stays free.
        assert!(!secret.push('\u{00e9}'));
        assert_eq!(secret.len(), Secret::MAX - 1);
        assert!(secret.push('x'));
    }

    #[test]
    fn from_str_truncates_rather_than_reallocating() {
        let long = "x".repeat(Secret::MAX + 100);
        let secret = Secret::from_str(&long);
        assert_eq!(secret.len(), Secret::MAX);
        assert_eq!(secret.bytes.capacity(), Secret::MAX);
    }

    #[test]
    fn pop_removes_whole_characters() {
        let mut secret = Secret::from_str("a\u{00e9}\u{1f600}");
        assert_eq!(secret.len(), 1 + 2 + 4);
        assert!(secret.pop());
        assert_eq!(secret.len(), 3);
        assert!(secret.pop());
        assert_eq!(secret.len(), 1);
        assert!(secret.pop());
        assert!(secret.is_empty());
        assert!(!secret.pop(), "an empty secret has nothing to pop");
    }

    // There is no test that the buffer is zero after `Drop` or after `pop`.
    // Reading a byte past `Vec::len`, or reading an allocation after it has
    // been freed, both need `unsafe`, and `src/lib.rs` forbids it. The
    // overwrite is asserted by review of `Drop` and `pop` above, and the
    // module documentation says the guarantee is best effort. Reported as
    // untestable rather than tested badly.

    #[test]
    fn clone_copies_the_bytes_and_keeps_the_capacity() {
        let secret = Secret::from_str("hunter2");
        let copy = secret.clone();
        assert_eq!(secret, copy);
        assert_eq!(copy.bytes.capacity(), Secret::MAX);
    }

    #[test]
    fn expose_str_is_none_for_bytes_that_are_not_utf8() {
        let mut secret = Secret::new();
        secret.bytes.push(0xff);
        assert_eq!(secret.expose_str(), None);
        assert_eq!(secret.expose(), &[0xff]);
    }

    /// S2, the half of it that lives in this file: this type has no `Display`
    /// and no `serde` impls, so nothing containing one can reach a TOML file.
    ///
    /// A compile-fail test is not available in the gate, so this reads its own
    /// source. It is deliberately literal: adding any of these three lines is
    /// then a deliberate edit to a test rather than an accident.
    #[test]
    fn secret_has_no_display_and_no_serde() {
        // Only the code above the test module is searched, and only the
        // lines that are code: the needles appear in this file's own
        // documentation and in this test, and a test that fails on its own
        // source is a test that can never pass.
        let source = include_str!("secret.rs");
        let code: String = production(source)
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in ["Display for Secret", "Serialize", "Deserialize"] {
            assert!(
                !code.contains(forbidden),
                "{forbidden} would make a secret printable or storable"
            );
        }
    }

    /// S5: every place a secret can escape is in `src/remote/`, and there are
    /// no more of them than the design budgets.
    ///
    /// The budget is an upper bound rather than an equality because the call
    /// sites are spread over files four agents write: an equality would fail
    /// for whoever landed first. One more than the budget still fails this
    /// test, which is what it is for.
    ///
    /// Only the code above each file's first `#[cfg(test)]` is counted, which
    /// is the house layout: tests go at the end of the file.
    #[test]
    fn the_only_places_a_secret_escapes_are_in_remote() {
        /// the design lists the places a secret can be, and the S5
        /// counts the calls that put it there.
        ///
        /// The contract writes **four**, counting "the argument to russh's or
        /// suppaftp's authenticate call" as one. It is five once both backends
        /// exist, and here is every one of them by name, so a sixth is a
        /// deliberate edit to this list and not an accident:
        ///
        /// 1. `keyring.rs` `SystemKeyring::set` - into the system keyring, and
        ///    only where the user opted in.
        /// 2. `sftp.rs` `try_key` - the passphrase handed to `ssh-key` to
        ///    decrypt a private key file.
        /// 3. `sftp.rs` `try_password` - the password handed to russh's
        ///    `authenticate_password`.
        /// 4. `ftp.rs` `open_extra` - the password re-sent to log in each
        ///    further connection of the pool.
        /// 5. `ftp.rs` the login attempt itself, handed to suppaftp's `login`.
        ///
        /// Every one of them borrows for the length of one call and copies
        /// nothing into an owned `String`.
        const EXPOSE_BUDGET: usize = 5;

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut total = 0usize;
        let mut offenders = Vec::new();
        for file in rust_sources(&root) {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let code = production(&text);
            let count = code.matches(".expose(").count() + code.matches(".expose_str(").count();
            if count == 0 {
                continue;
            }
            total += count;
            if !file.starts_with(root.join("remote")) {
                offenders.push(file.display().to_string());
            }
        }
        assert!(
            offenders.is_empty(),
            "a secret is exposed outside src/remote/: {offenders:?}"
        );
        assert!(
            total <= EXPOSE_BUDGET,
            "{total} call sites expose a secret; the contract budgets {EXPOSE_BUDGET}"
        );
    }

    /// The part of a source file above its test module.
    ///
    /// Cut at the first **line** that is the `#[cfg(test)]` attribute, not at
    /// the first occurrence of that text: this module's own documentation
    /// mentions the attribute, and cutting there would silently make both
    /// censuses below scan nothing and pass for the wrong reason.
    fn production(source: &str) -> String {
        source
            .lines()
            .take_while(|line| !line.trim_start().starts_with("#[cfg(test)]"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Every `.rs` file under a directory, recursively, without following
    /// symlinks. Used by the census above.
    fn rust_sources(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return found;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(rust_sources(&path));
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                found.push(path);
            }
        }
        found
    }
}
