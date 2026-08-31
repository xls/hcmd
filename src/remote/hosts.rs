//! `hosts.toml`: the host book.
//!
//! the design fixes the file's shape and, more importantly, what is **not**
//! in it:
//!
//! > A password must never be written to `hosts.toml` or any other file in
//! > `~/.config`. [...] `hosts.toml` therefore holds only non-secret fields.
//!
//! ```toml
//! [[host]]
//! label      = "nas"
//! protocol   = "sftp"
//! host       = "nas.local"
//! port       = 2222
//! username   = "thorin"
//! auth       = "agent"          # agent | key | password | keyring
//! key_file   = "~/.ssh/id_ed25519"
//! remote_dir = "/srv/media"
//! local_dir  = "~/Downloads"
//! ```
//!
//! # There is no password field and there never is one
//!
//! [`SavedHost`] has nine fields and none of them can hold a credential. A
//! `password` key in a hand-written file is read, **discarded**, and warned
//! about by name, so a user who tried it finds out rather than believing it
//! worked (the design S2, S8). The type that holds a credential in
//! memory (`crate::remote::secret`) implements neither `Serialize` nor
//! `Deserialize`, so no future edit can put one in this file even by accident -
//! and the name of that type does not occur anywhere in this module, which a
//! test in this file asserts by reading its own source.
//!
//! # Nothing here can fail the dialog
//!
//! the rule for every configuration file: a missing file, an
//! unreadable file, a file that is not TOML, one `[[host]]` that will not
//! deserialise - all of them degrade to a warning and the entries that did
//! parse. [`load`] never returns an error. Only [`store`] does, because a
//! `Save` that silently did nothing would be worse than one that says why.
//!
//! Each `[[host]]` is deserialised **on its own** rather than the file being
//! deserialised whole, which is what makes "warns and keeps the rest" true of
//! a bad `auth` value and not only of an unknown key.
//!
//!
//! # `~` expands at use, not at load
//!
//! `key_file` and `local_dir` keep the text the user typed and are expanded
//! against `$HOME` when they are used ([`expand_tilde`]), so a `hosts.toml`
//! copied between machines keeps working.
//!
//! # This file is not generated
//!
//! `hosts.toml` is **not** one of the generated files: it is a list
//! the user builds, like `searches.toml`, and a commented-out example host
//! would be a host in the list. It is created when the first host is saved and
//! not before.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::config_dir;
use crate::error::{Error, Result};
use crate::remote::{Protocol, Target};

/// The host book, in the config directory beside `searches.toml`.
pub const HOSTS_FILE: &str = "hosts.toml";

/// The `auth` field's four values, and nothing else.
///
/// The vocabulary is shared with [`crate::remote::auth::Method::id`], so a
/// message and a file say the same word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    /// `agent`: the SSH agent, then the default keys, then a prompt.
    /// the design tries the agent first always, so this is also the default.
    #[default]
    Agent,
    /// `key`: the key named by `key_file`.
    Key,
    /// `password`: prompt at connect time, hold it for the session only.
    Password,
    /// `keyring`: the per-host opt-in of the method 4. The
    /// password itself lives in the system keyring and never here.
    Keyring,
}

impl AuthMethod {
    /// The `hosts.toml` spelling: `"agent"`, `"key"`, `"password"`,
    /// `"keyring"`.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Key => "key",
            Self::Password => "password",
            Self::Keyring => "keyring",
        }
    }

    /// Every value, in the order the Add-host form steps through them.
    pub const ALL: &'static [Self] = &[Self::Agent, Self::Key, Self::Password, Self::Keyring];

    /// Parse a `hosts.toml` value. `None` for anything else, which the caller
    /// turns into a warning rather than a silent default.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "agent" => Some(Self::Agent),
            "key" => Some(Self::Key),
            "password" => Some(Self::Password),
            "keyring" => Some(Self::Keyring),
            _ => None,
        }
    }
}

impl std::fmt::Display for AuthMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.id())
    }
}

/// One `[[host]]`.
///
/// **There is no password field and there never is one.** Adding one would be
/// a security regression, not a feature (the design S2).
///
/// `Debug` is derived and that is safe because every field is a non-secret:
/// this type is one of the four the design calls "only non-secret fields",
/// and the design S3 tests that it stays that way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedHost {
    /// What the connect dialog lists it under, and what a delete confirmation
    /// names.
    pub label: String,
    /// `sftp`, `ftp`, `ftps` or `ftps-implicit`.
    pub protocol: Protocol,
    /// The hostname or address. An entry with none of this is kept, warned
    /// about, and refused at `Connect`.
    pub host: String,
    /// The port. `0` means "the protocol's default", which is what an omitted
    /// key deserialises to and what [`parse`] rewrites before anyone sees it.
    pub port: u16,
    /// The login name. Empty means the quick-connect default: `$USER` for the
    /// SSH family, `anonymous` for FTP.
    pub username: String,
    /// Which of the four methods this host prefers.
    pub auth: AuthMethod,
    /// `~/.ssh/id_ed25519`. Kept as text; `~` expands at use ([`expand_tilde`]).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub key_file: String,
    /// The initial remote directory, or empty for "wherever the server puts
    /// us".
    #[serde(skip_serializing_if = "String::is_empty")]
    pub remote_dir: String,
    /// the "initial local directory for the other panel". Kept as
    /// text; `~` expands at use.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub local_dir: String,
}

impl Default for SavedHost {
    /// An empty host on the default protocol, which is what the
    /// Add-host form opens on and what a `[[host]]` with missing keys fills
    /// itself in from.
    ///
    /// `port` is `0` rather than `22` so that a `[[host]]` that names `ftp` and
    /// omits the port gets 21 and not SSH's 22: the default port is a function
    /// of the protocol, and the protocol is not known until the whole table has
    /// been read (see [`parse`]).
    fn default() -> Self {
        Self {
            label: String::new(),
            protocol: Protocol::Sftp,
            host: String::new(),
            port: 0,
            username: String::new(),
            auth: AuthMethod::Agent,
            key_file: String::new(),
            remote_dir: String::new(),
            local_dir: String::new(),
        }
    }
}

impl SavedHost {
    /// Every key this file understands, for the unknown-key warning.
    ///
    /// Written out rather than derived, because serde's `deny_unknown_fields`
    /// would refuse the whole file and the design wants a warning and the rest
    /// of the list.
    pub const KEYS: &'static [&'static str] = &[
        "label",
        "protocol",
        "host",
        "port",
        "username",
        "auth",
        "key_file",
        "remote_dir",
        "local_dir",
    ];

    /// The port to dial: the one that was set, or the protocol's default.
    pub fn effective_port(&self) -> u16 {
        if self.port == 0 {
            self.protocol.default_port()
        } else {
            self.port
        }
    }

    /// Where this host points, with no secret in it (the design,
    /// S3).
    pub fn target(&self) -> Target {
        Target {
            protocol: self.protocol,
            host: self.host.clone(),
            port: self.effective_port(),
            user: self.username.clone(),
            dir: (!self.remote_dir.is_empty()).then(|| self.remote_dir.clone()),
        }
    }

    /// The second column of the saved-host list:
    /// `sftp://thorin@nas.local:2222/srv/media`.
    ///
    /// Built from [`Target`] rather than formatted here, so the list and the
    /// panel header of the design cannot disagree about what a connection
    /// is called.
    pub fn summary(&self) -> String {
        let target = self.target();
        if self.remote_dir.is_empty() {
            target.authority()
        } else {
            target.url(&self.remote_dir)
        }
    }

    /// The key file as a path, or `None` when the field is empty.
    pub fn key_path(&self, home: &Path) -> Option<PathBuf> {
        (!self.key_file.is_empty()).then(|| expand_tilde(&self.key_file, home))
    }

    /// the initial local directory for the other panel, or `None`.
    pub fn local_path(&self, home: &Path) -> Option<PathBuf> {
        (!self.local_dir.is_empty()).then(|| expand_tilde(&self.local_dir, home))
    }

    /// Why this entry cannot be connected to, phrased for a dialog's error
    /// row, or `None` when it can.
    ///
    /// Refusing here is what keeps the "refused up front" true of the
    /// host book: an entry with no host would otherwise fail after a dialog has
    /// closed and a connection has been attempted.
    pub fn problem(&self) -> Option<String> {
        if self.host.trim().is_empty() {
            return Some("a host needs an address".to_string());
        }
        if self.label.trim().is_empty() {
            return Some("a host needs a label".to_string());
        }
        if self.auth == AuthMethod::Key && self.key_file.trim().is_empty() {
            return Some("auth = key needs a key file".to_string());
        }
        None
    }
}

/// Expand a leading `~` against `home`.
///
/// At **use** and not at load, so a `hosts.toml` copied between machines keeps
/// working. Only a leading `~/`, and a bare `~`, are expanded: `~thorin` is
/// another user's home directory, which this program does not resolve, and
/// silently turning it into `$HOME/thorin` would name the wrong file.
pub fn expand_tilde(text: &str, home: &Path) -> PathBuf {
    match text.strip_prefix('~') {
        Some("") => home.to_path_buf(),
        Some(rest) => match rest.strip_prefix('/') {
            Some(tail) => home.join(tail),
            // `~thorin/...`: not ours to expand.
            None => PathBuf::from(text),
        },
        None => PathBuf::from(text),
    }
}

/// The file as a whole: an array of tables called `host`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct HostsFile {
    host: Vec<SavedHost>,
}

/// Where `hosts.toml` lives: `~/.config/holoscommander/hosts.toml`.
pub fn hosts_path() -> Result<PathBuf> {
    Ok(config_dir()?.join(HOSTS_FILE))
}

/// Parse `hosts.toml`'s text into the entries that could be read and a warning
/// per thing that could not.
///
/// Every `[[host]]` is deserialised on its own, so one bad `auth` value costs
/// one entry rather than the file.
pub fn parse(text: &str) -> (Vec<SavedHost>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let table = match toml::from_str::<toml::Table>(text) {
        Ok(table) => table,
        Err(err) => {
            warnings.push(format!("{HOSTS_FILE}: {err}; no saved hosts loaded"));
            return (Vec::new(), warnings);
        }
    };
    for key in table.keys() {
        if key != "host" {
            warnings.push(format!("{HOSTS_FILE}: unknown section `{key}`, ignored"));
        }
    }
    let Some(entries) = table.get("host") else {
        // An empty or absent list is the normal first run, not a warning.
        return (Vec::new(), warnings);
    };
    let Some(entries) = entries.as_array() else {
        warnings.push(format!(
            "{HOSTS_FILE}: `host` is not an array of tables; no saved hosts loaded"
        ));
        return (Vec::new(), warnings);
    };

    let mut hosts: Vec<SavedHost> = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        // One-based, because that is how a person counts `[[host]]` blocks.
        let at = index.saturating_add(1);
        let Some(fields) = entry.as_table() else {
            warnings.push(format!(
                "{HOSTS_FILE}: [[host]] {at} is not a table, ignored"
            ));
            continue;
        };
        for key in fields.keys() {
            if key == "password" {
                // said out loud rather than dropped in silence:
                // a user who wrote one here has to learn that it did nothing.
                warnings.push(format!(
                    "{HOSTS_FILE}: [[host]] {at}: `password` is ignored and was not read; \
 keeps passwords in the system keyring, never in a file \
                     under ~/.config"
                ));
            } else if !SavedHost::KEYS.contains(&key.as_str()) {
                warnings.push(format!(
                    "{HOSTS_FILE}: [[host]] {at}: unknown key `{key}`, ignored"
                ));
            }
        }
        let mut host: SavedHost = match entry.clone().try_into() {
            Ok(host) => host,
            Err(err) => {
                warnings.push(format!("{HOSTS_FILE}: [[host]] {at}: {err}; entry ignored"));
                continue;
            }
        };
        host.label = host.label.trim().to_string();
        host.host = host.host.trim().to_string();
        host.username = host.username.trim().to_string();
        // The default port is a function of the protocol, so it is resolved
        // here rather than in `Default`, which does not know the protocol yet.
        if host.port == 0 {
            host.port = host.protocol.default_port();
        }
        if let Some(why) = host.problem() {
            let name = if host.label.is_empty() {
                format!("[[host]] {at}")
            } else {
                format!("[[host]] {at} ({})", host.label)
            };
            // Kept, not dropped: `store` rewrites the whole file, so dropping
            // an entry here would delete a line the user typed.
            warnings.push(format!("{HOSTS_FILE}: {name}: {why}"));
        }
        hosts.push(host);
    }
    (hosts, warnings)
}

/// Render a host book back to TOML.
///
/// The inverse of [`parse`] for every host [`parse`] accepts, which is what
/// makes editing one entry in the Add-host form safe for the other twenty.
pub fn render(hosts: &[SavedHost]) -> Result<String> {
    let file = HostsFile {
        host: hosts.to_vec(),
    };
    toml::to_string_pretty(&file).map_err(|e| Error::msg(format!("{HOSTS_FILE}: {e}")))
}

/// Read the host book, degrading to an empty list and a warning on every
/// failure.
pub fn load() -> (Vec<SavedHost>, Vec<String>) {
    let Ok(path) = hosts_path() else {
        return (Vec::new(), Vec::new());
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text),
        // A missing file is the normal first run: the file is created when the
        // first host is saved and not before.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (Vec::new(), Vec::new()),
        Err(err) => (Vec::new(), vec![format!("{}: {err}", path.display())]),
    }
}

/// Write the host book, creating the config directory if needed.
///
/// **Called from the event loop, never from `Dialog::handle_key`**
/// (the design): the Add-host form
/// and `F8` change a *list*, and the write happens where every other write
/// happens.
pub fn store(hosts: &[SavedHost]) -> Result<()> {
    let path = hosts_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    }
    let text = render(hosts)?;
    std::fs::write(&path, text).map_err(|e| Error::io(&path, e))
}

/// The saved hosts as last read from or written to `hosts.toml`, and whether
/// the two still agree.
///
/// A password is never in it: the design keeps secrets out of
/// `~/.config` entirely, and the type that holds the book in memory is the
/// place that claim has to stay true.
///
/// The dirty flag is a flag rather than a write because
/// [`crate::input::dispatch`] may not touch the filesystem: the Add-host form
/// and `F8` change a list, and the write happens in the event loop where every
/// other write happens.
#[derive(Debug, Default, Clone)]
pub struct Book {
    saved: Vec<SavedHost>,
    dirty: bool,
}

impl Book {
    /// The hosts, in the order the file holds them.
    pub fn hosts(&self) -> &[SavedHost] {
        &self.saved
    }

    /// The hosts, mutably, for the one caller that edits an entry in place.
    ///
    /// Marks the book dirty: anything that can change an entry has changed the
    /// book, and a caller that took a `&mut` and then decided against writing
    /// would cost one harmless rewrite of the file.
    pub fn hosts_mut(&mut self) -> &mut Vec<SavedHost> {
        self.dirty = true;
        &mut self.saved
    }

    /// Replace the book, as the connect dialog's answer does.
    pub fn replace(&mut self, hosts: Vec<SavedHost>) {
        self.saved = hosts;
        self.dirty = true;
    }

    /// Adopt what `load` read. Not dirty: the file and memory agree by
    /// construction at this point.
    pub fn adopt(&mut self, hosts: Vec<SavedHost>) {
        self.saved = hosts;
        self.dirty = false;
    }

    /// Do the file and memory still disagree?
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Write the book back if it has changed, and say what went wrong if it
    /// could not be written.
    ///
    /// The flag is cleared either way: a write that failed is reported once,
    /// not retried on every frame for the rest of the session.
    pub fn store_if_dirty(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        self.dirty = false;
        store(&self.saved)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_protocol_survives_the_hosts_file() {
        // hosts.toml uses serde's derive and `id()`/`parse()` use a different
        // spelling (`s3+http` versus `s3http`); as long as each is symmetric
        // with itself a host round-trips, and this is what says so for every
        // protocol at once, so a new one cannot be saved and come back wrong.
        for protocol in Protocol::ALL {
            let host = SavedHost {
                label: "h".to_string(),
                protocol: *protocol,
                host: "example.invalid".to_string(),
                port: 1234,
                username: "u".to_string(),
                auth: AuthMethod::Keyring,
                key_file: String::new(),
                remote_dir: String::new(),
                local_dir: String::new(),
            };
            let toml = toml::to_string(&host).expect("serialize");
            let back: SavedHost = toml::from_str(&toml).expect("deserialize");
            assert_eq!(
                back.protocol, *protocol,
                "{protocol:?} came back as {:?}\n{toml}",
                back.protocol
            );
        }
    }

    use super::*;

    /// the own example, character for character.
    const DOCUMENTED_EXAMPLE: &str = r#"
[[host]]
label      = "nas"
protocol   = "sftp"
host       = "nas.local"
port       = 2222
username   = "thorin"
auth       = "agent"          # agent | key | password | keyring
key_file   = "~/.ssh/id_ed25519"
remote_dir = "/srv/media"
local_dir  = "~/Downloads"
"#;

    fn nas() -> SavedHost {
        SavedHost {
            label: "nas".to_string(),
            protocol: Protocol::Sftp,
            host: "nas.local".to_string(),
            port: 2222,
            username: "thorin".to_string(),
            auth: AuthMethod::Agent,
            key_file: "~/.ssh/id_ed25519".to_string(),
            remote_dir: "/srv/media".to_string(),
            local_dir: "~/Downloads".to_string(),
        }
    }

    #[test]
    fn spec_16_3s_example_parses_to_exactly_what_it_says() {
        let (hosts, warnings) = parse(DOCUMENTED_EXAMPLE);
        assert_eq!(warnings, Vec::<String>::new(), "the example is clean");
        assert_eq!(hosts, vec![nas()]);
    }

    #[test]
    fn a_host_book_survives_a_round_trip() {
        // parse, render, parse, compare: the property the Add-host form relies
        // on when it rewrites the whole file to change one entry.
        let book = vec![
            nas(),
            SavedHost {
                label: "buildbox".to_string(),
                protocol: Protocol::Sftp,
                host: "buildbox".to_string(),
                port: 22,
                username: "thorin".to_string(),
                auth: AuthMethod::Key,
                key_file: "~/.ssh/id_ecdsa".to_string(),
                ..SavedHost::default()
            },
            SavedHost {
                label: "mirror".to_string(),
                protocol: Protocol::Ftp,
                host: "ftp.example.org".to_string(),
                port: 21,
                username: "anonymous".to_string(),
                auth: AuthMethod::Password,
                ..SavedHost::default()
            },
            SavedHost {
                label: "vault".to_string(),
                protocol: Protocol::FtpsImplicit,
                host: "vault.example.org".to_string(),
                port: 990,
                username: "thorin".to_string(),
                auth: AuthMethod::Keyring,
                ..SavedHost::default()
            },
        ];
        let text = render(&book).expect("render");
        let (back, warnings) = parse(&text);
        assert_eq!(warnings, Vec::<String>::new(), "{text}");
        assert_eq!(back, book, "{text}");

        // And a second trip is byte-identical, so saving twice does not churn
        // the file.
        assert_eq!(render(&back).expect("render"), text);
    }

    #[test]
    fn a_password_key_is_warned_about_by_name_and_never_read() {
        // the design and the design S2: the value must not reach
        // a `SavedHost`, and the user must be told rather than left believing
        // it worked.
        let text = r#"
[[host]]
label    = "nas"
host     = "nas.local"
username = "thorin"
password = "hunter2"
"#;
        let (hosts, warnings) = parse(text);
        assert_eq!(hosts.len(), 1, "the rest of the entry is kept");
        let rendered = format!("{hosts:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(
            !render(&hosts).expect("render").contains("hunter2"),
            "and it cannot be written back out"
        );
        assert!(
            warnings.iter().any(|w| w.contains("`password` is ignored")),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().all(|w| !w.contains("hunter2")),
            "and the warning does not quote it back: {warnings:?}"
        );
    }

    #[test]
    fn no_field_of_a_saved_host_is_called_password() {
        // the design S2, the half a compile-fail test would cover:
        // every key `render` can emit, listed.
        let full = SavedHost {
            label: "l".to_string(),
            protocol: Protocol::Sftp,
            host: "h".to_string(),
            port: 22,
            username: "u".to_string(),
            auth: AuthMethod::Keyring,
            key_file: "k".to_string(),
            remote_dir: "r".to_string(),
            local_dir: "d".to_string(),
        };
        let text = render(&[full]).expect("render");
        let keys: Vec<&str> = text
            .lines()
            .filter_map(|line| line.split_once('=').map(|(k, _)| k.trim()))
            .collect();
        assert!(!keys.is_empty());
        for key in &keys {
            assert!(
                SavedHost::KEYS.contains(key),
                "{key} is not one of the nine non-secret fields: {keys:?}"
            );
        }
        assert!(!keys.contains(&"password"), "{keys:?}");
    }

    #[test]
    fn this_module_cannot_name_the_type_that_holds_a_credential() {
        // the design S2's grep-shaped half: a future edit that
        // gives `SavedHost` a secret field has to defeat this test first.
        //
        // The needle is assembled rather than written, because the assertion
        // reads its own source and a literal spelling would find itself.
        let needle = concat!("Sec", "ret");
        let source = include_str!("hosts.rs");
        assert!(
            !source.contains(needle),
            "hosts.rs names the in-memory credential type"
        );
    }

    #[test]
    fn an_unknown_key_warns_and_keeps_the_rest() {
        let text = r#"
[[host]]
label   = "nas"
host    = "nas.local"
colour  = "blue"
"#;
        let (hosts, warnings) = parse(text);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts.first().map(|h| h.host.as_str()), Some("nas.local"));
        assert!(
            warnings.iter().any(|w| w.contains("unknown key `colour`")),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_bad_value_costs_one_entry_and_not_the_file() {
        // Each `[[host]]` is deserialised on its own, which is what makes
        // the "a warning, never a failure" true per entry.
        let text = r#"
[[host]]
label = "good"
host  = "nas.local"

[[host]]
label = "bad"
host  = "other.local"
auth  = "telepathy"

[[host]]
label = "also good"
host  = "third.local"
"#;
        let (hosts, warnings) = parse(text);
        let labels: Vec<&str> = hosts.iter().map(|h| h.label.as_str()).collect();
        assert_eq!(labels, vec!["good", "also good"]);
        assert!(
            warnings.iter().any(|w| w.contains("[[host]] 2")),
            "the warning names which block: {warnings:?}"
        );
    }

    #[test]
    fn an_omitted_port_is_the_protocols_own_default() {
        let text = r#"
[[host]]
label    = "nas"
host     = "nas.local"

[[host]]
label    = "mirror"
protocol = "ftp"
host     = "ftp.example.org"

[[host]]
label    = "vault"
protocol = "ftps-implicit"
host     = "vault.example.org"
"#;
        let (hosts, warnings) = parse(text);
        let ports: Vec<u16> = hosts.iter().map(|h| h.port).collect();
        assert_eq!(ports, vec![22, 21, 990]);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_file_that_is_not_toml_is_a_warning_and_an_empty_list() {
        // the connect dialog opens either way.
        let (hosts, warnings) = parse("[[host]\nlabel = ");
        assert!(hosts.is_empty());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains(HOSTS_FILE)));

        // An empty file is not a warning at all.
        assert_eq!(parse(""), (Vec::new(), Vec::new()));
    }

    #[test]
    fn an_entry_with_no_address_is_kept_and_warned_about() {
        // Kept: `store` rewrites the whole file, so dropping it here would
        // delete a line the user typed.
        let (hosts, warnings) = parse("[[host]]\nlabel = \"nowhere\"\n");
        assert_eq!(hosts.len(), 1);
        assert!(
            warnings.iter().any(|w| w.contains("needs an address")),
            "{warnings:?}"
        );
        assert!(hosts.first().and_then(SavedHost::problem).is_some());
    }

    #[test]
    fn a_tilde_expands_at_use_and_only_when_it_is_ours_to_expand() {
        let home = Path::new("/home/thorin");
        assert_eq!(
            expand_tilde("~/.ssh/id_ed25519", home),
            PathBuf::from("/home/thorin/.ssh/id_ed25519")
        );
        assert_eq!(expand_tilde("~", home), PathBuf::from("/home/thorin"));
        assert_eq!(
            expand_tilde("/etc/ssh/key", home),
            PathBuf::from("/etc/ssh/key")
        );
        // Another user's home is not ours to guess at.
        assert_eq!(
            expand_tilde("~root/.ssh/id_rsa", home),
            PathBuf::from("~root/.ssh/id_rsa")
        );
        // And the text on the host is untouched by loading: the round trip
        // above already proves it, this proves the accessor expands.
        let host = nas();
        assert_eq!(
            host.key_path(home),
            Some(PathBuf::from("/home/thorin/.ssh/id_ed25519"))
        );
        assert_eq!(
            host.local_path(home),
            Some(PathBuf::from("/home/thorin/Downloads"))
        );
        assert_eq!(SavedHost::default().key_path(home), None);
    }

    #[test]
    fn a_saved_host_summarises_as_the_url_the_panel_header_shows() {
        // the list column and the header are the same
        // string, built in one place.
        assert_eq!(nas().summary(), "sftp://thorin@nas.local:2222/srv/media");
        let no_dir = SavedHost {
            remote_dir: String::new(),
            ..nas()
        };
        assert_eq!(no_dir.summary(), "sftp://thorin@nas.local:2222");
    }

    #[test]
    fn the_auth_vocabulary_is_the_four_words_spec_16_3_uses() {
        for method in AuthMethod::ALL {
            assert_eq!(AuthMethod::parse(method.id()), Some(*method));
            assert_eq!(method.to_string(), method.id());
        }
        assert_eq!(AuthMethod::ALL.len(), 4);
        assert_eq!(AuthMethod::parse("keyring "), None);
        assert_eq!(AuthMethod::parse("Agent"), None);
        assert_eq!(AuthMethod::default(), AuthMethod::Agent);
    }

    #[test]
    fn a_host_that_needs_a_key_file_and_has_none_is_refused_up_front() {
        let host = SavedHost {
            label: "buildbox".to_string(),
            host: "buildbox".to_string(),
            auth: AuthMethod::Key,
            ..SavedHost::default()
        };
        assert_eq!(
            host.problem().as_deref(),
            Some("auth = key needs a key file")
        );
    }
}
