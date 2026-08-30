//! Resolving uids and gids to names, cached.
//!
//! # Why this reads `/etc/passwd` directly
//!
//! Nothing in this project shells out, so `id -un` is not an
//! option, and `getpwuid_r` needs a `libc` dependency that rule 5
//! does not justify for two columns that are not in the default layout. The
//! files are a documented, stable format and reading them is about twenty
//! lines.
//!
//! **The limitation this accepts**: users that exist only in LDAP, SSSD, NIS or
//! systemd-homed are not in `/etc/passwd`, so they resolve to their numeric id.
//! That is the same thing the column would show with no resolution at all, so
//! it degrades to exactly the previous behaviour rather than to something
//! wrong. A `nss`-backed lookup is the fix if it ever matters, and it is a
//! change behind these two functions.
//!
//! # Caching
//!
//! Both files are read **once per process**, on first use, into a map. A file
//! manager session is short next to the rate at which accounts change, and the
//! alternative - re-reading per rendered row - is a syscall storm on every
//! frame. `OnceLock` also makes the read happen off whatever thread asks first
//! without a lock on the read path afterwards.

use std::collections::HashMap;
use std::sync::OnceLock;

/// The `/etc/passwd` map, built on first use.
static USERS: OnceLock<HashMap<u32, String>> = OnceLock::new();
/// The `/etc/group` map, built on first use.
static GROUPS: OnceLock<HashMap<u32, String>> = OnceLock::new();

/// Parse a `passwd`/`group`-shaped file: colon-separated, name first, with the
/// numeric id at zero-based field `id_index`.
///
/// Both files put the id third - `name:passwd:uid:gid:…` and
/// `name:passwd:gid:members` - so `id_index` is 2 for each, but it is a
/// parameter rather than a constant because the two files only *happen* to
/// agree and a reader should not have to take that on trust.
///
/// Anything malformed is skipped rather than being an error - a single bad line
/// in `/etc/passwd` must not cost the whole map.
fn parse(text: &str, id_index: usize) -> HashMap<u32, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split(':').collect();
        let Some(name) = fields.first().filter(|n| !n.is_empty()) else {
            continue;
        };
        let Some(id) = fields
            .get(id_index)
            .and_then(|f| f.trim().parse::<u32>().ok())
        else {
            continue;
        };
        // First entry wins, which is what a name lookup does: two lines sharing
        // an id are aliases and the first is the canonical one.
        out.entry(id).or_insert_with(|| (*name).to_string());
    }
    out
}

/// The zero-based field holding the id in both `/etc/passwd` and `/etc/group`.
const ID_FIELD: usize = 2;

fn users() -> &'static HashMap<u32, String> {
    USERS.get_or_init(|| {
        std::fs::read_to_string("/etc/passwd")
            .map(|text| parse(&text, ID_FIELD))
            .unwrap_or_default()
    })
}

fn groups() -> &'static HashMap<u32, String> {
    GROUPS.get_or_init(|| {
        std::fs::read_to_string("/etc/group")
            .map(|text| parse(&text, ID_FIELD))
            .unwrap_or_default()
    })
}

/// The name for a uid, or the number as text when it cannot be resolved.
///
/// Never fails and never blocks on anything but the first read.
pub fn owner_name(uid: u32) -> String {
    users()
        .get(&uid)
        .cloned()
        .unwrap_or_else(|| uid.to_string())
}

/// The name for a gid, or the number as text.
pub fn group_name(gid: u32) -> String {
    groups()
        .get(&gid)
        .cloned()
        .unwrap_or_else(|| gid.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every uid here differs from the gid on the same line, so a parser that
    // read the fourth field instead of the third would fail rather than pass by
    // coincidence. It did.
    const PASSWD: &str = "\
root:x:0:10:root:/root:/bin/bash
# a comment
bin:x:1:11::/:/usr/bin/nologin

thorin:x:1000:1001:Thorin:/home/thorin:/bin/bash
broken-line-with-no-colons
nobody:x:65534:65500:Nobody:/:/usr/bin/nologin
alias:x:0:10:an alias for root:/root:/bin/bash
";

    #[test]
    fn passwd_parses_to_uid_names() {
        let map = parse(PASSWD, ID_FIELD);
        assert_eq!(map.get(&0).map(String::as_str), Some("root"));
        assert_eq!(map.get(&1000).map(String::as_str), Some("thorin"));
        assert_eq!(map.get(&65534).map(String::as_str), Some("nobody"));
        // The gid column of /etc/passwd is not what this map holds.
        assert_eq!(map.get(&1001), None, "1001 is thorin's gid, not a uid");
    }

    #[test]
    fn a_malformed_line_costs_only_itself() {
        let map = parse(PASSWD, ID_FIELD);
        // Four good uids: 0, 1, 1000, 65534. The comment, the blank line and
        // the colon-less line contributed nothing, and none of them stopped
        // the lines after them being read.
        assert_eq!(map.len(), 4, "{map:?}");
    }

    #[test]
    fn the_first_entry_for_a_uid_wins() {
        let map = parse(PASSWD, ID_FIELD);
        assert_eq!(map.get(&0).map(String::as_str), Some("root"));
    }

    #[test]
    fn a_group_file_parses_the_same_way() {
        let map = parse("wheel:x:998:thorin\nusers:x:100:\n", ID_FIELD);
        assert_eq!(map.get(&998).map(String::as_str), Some("wheel"));
        assert_eq!(map.get(&100).map(String::as_str), Some("users"));
    }

    #[test]
    fn an_unknown_id_falls_back_to_the_number() {
        // 4294967294 is not going to be in /etc/passwd on the test machine.
        assert_eq!(owner_name(u32::MAX.saturating_sub(1)), "4294967294");
        assert_eq!(group_name(u32::MAX.saturating_sub(1)), "4294967294");
    }

    #[test]
    fn root_resolves_on_a_real_system() {
        // Every Unix has uid 0. If /etc/passwd could not be read at all this
        // degrades to "0", which is still correct output, so the assertion is
        // written to accept either rather than to fail in a sandbox.
        let name = owner_name(0);
        assert!(name == "root" || name == "0", "unexpected: {name}");
    }
}
