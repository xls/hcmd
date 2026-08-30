//! The device picker's model.
//!
//! Enumeration is `sysinfo`, in process: the design rule out an
//! `lsblk` subprocess. See the design for what `sysinfo`
//! filters on its own account and 2.4 for the udisks2 half that is deferred.
//!
//! # What is here and what is not
//!
//! Everything in this module except [`enumerate`] and [`labels`] is pure, so
//! the row and the filter are tested from synthetic
//! [`Device`] values on a machine with no removable volume on it.
//! The two that read the machine are called
//! from the event loop only, because reading the mount table is I/O and
//! `crate::input::dispatch` may not do any.
//!
//! # The device node is never shown
//!
//! the design settled it and `crate::ui::volume::Volume::device` records the
//! reasoning: a LUKS or LVM mapper is routinely named `root`, which reads as
//! the superuser and says nothing about where you are. [`Device`] therefore
//! has no `device` field at all; [`labels`] uses the node internally to find
//! the label udev published for it and does not publish the node itself.

pub mod hotlist;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::ui::text::{self, Crop};
use crate::ui::volume::human;

/// The drive popup's place in the event loop: one request slot and one
/// deadline.
///
/// The mount table is read by the event loop and never by
/// [`crate::input::dispatch`], and it is polled again only while the popup
/// that shows it is open. Both halves live here so that closing the popup and
/// disarming the poll are the same act.
///
/// There is one request slot, so `Alt+F1` then `Alt+F2` before a frame has
/// been drawn opens the right panel's popup: the last key the user pressed is
/// the one that wins.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Drives {
    asked: Option<crate::app::DrivesRequest>,
    poll_at: Option<Instant>,
}

impl Drives {
    /// Queue a popup, replacing whatever was asked for and not yet built.
    pub const fn ask(&mut self, request: crate::app::DrivesRequest) {
        self.asked = Some(request);
    }

    /// Which popup is queued, if any.
    pub const fn asked(&self) -> Option<crate::app::DrivesRequest> {
        self.asked
    }

    /// Take the queued popup, so the event loop can build it.
    pub const fn take(&mut self) -> Option<crate::app::DrivesRequest> {
        self.asked.take()
    }

    /// Arm the re-enumeration deadline for a popup that is now open.
    pub fn arm(&mut self, now: Instant) {
        self.poll_at = Some(now + POLL);
    }

    /// Stop polling: the popup is closed, or shows nothing that can change.
    pub const fn disarm(&mut self) {
        self.poll_at = None;
    }

    /// When the next re-enumeration is due, if one is.
    pub const fn deadline(&self) -> Option<Instant> {
        self.poll_at
    }

    /// Is a re-enumeration due at `now`?
    pub fn is_due(&self, now: Instant) -> bool {
        self.poll_at.is_some_and(|due| now >= due)
    }
}

/// One mounted filesystem, as the popup shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// Where it is mounted. Also the path the panel goes to.
    pub mount_point: String,
    /// The filesystem label where udev published one, else the mount point.
    pub label: String,
    /// `ext4`, `btrfs`, `vfat`.
    pub fs_type: String,
    /// Free bytes.
    pub free: u64,
    /// Total bytes.
    pub total: u64,
    /// `sysinfo`'s removable flag ("Removable volumes are
    /// marked").
    pub removable: bool,
    /// Mounted read-only, shown as a marker beside the removable one.
    pub read_only: bool,
}

/// How often an open popup re-enumerates (the live list; see
/// the design for why this is a poll and not `notify`).
pub const POLL: Duration = Duration::from_secs(1);

/// The most rows the design draws without a scrollbar.
pub const MAX_VISIBLE_ROWS: usize = 9;

/// Filesystem types hcmd hides unless `devices.show_all`.
///
/// A second line of defence over what `sysinfo` already drops, plus the names
/// it does not.
pub const PSEUDO_FILESYSTEMS: &[&str] = &[
    "proc",
    "sysfs",
    "cgroup",
    "cgroup2",
    "devtmpfs",
    "devpts",
    "squashfs",
    "overlay",
    "tracefs",
    "debugfs",
    "configfs",
    "securityfs",
    "bpf",
    "efivarfs",
    "binfmt_misc",
    "fusectl",
    "fuse.portal",
    "mqueue",
    "hugetlbfs",
    "pstore",
    "ramfs",
    "rpc_pipefs",
    "autofs",
];

/// Mount points whose subtrees are the kernel's own, whatever is mounted on
/// them.
///
/// `/run` is not here: the design hides "`tmpfs` under `/run`" but
/// `/run/media`, which is where udisks2 mounts a USB stick, is the one place
/// under it a user does want to reach. [`is_pseudo`] handles that pair on its
/// own.
const PSEUDO_MOUNTS: &[&str] = &["/proc", "/sys", "/dev"];

/// Where udev publishes the filesystem labels (the "volume label").
const BY_LABEL: &str = "/dev/disk/by-label";

/// The marker on a removable volume ("Removable volumes are
/// marked"), and its `ui.ascii_borders` spelling.
const REMOVABLE: &str = "\u{23cf}";
/// [`REMOVABLE`]'s ASCII spelling. The eject symbol is not in
/// `crate::ui::text`'s table because nothing else in the program draws one.
const REMOVABLE_ASCII: &str = "[r]";
/// The marker on a read-only mount. Already ASCII, in both glyph sets: `ro` is
/// what `mount` itself calls it, and inventing a symbol for it would teach a
/// second vocabulary for a word that already fits.
const READ_ONLY: &str = "[ro]";

/// How wide the gap between two fields of a row is.
const GAP: &str = "  ";

/// Whether hcmd's own filter hides this mount.
///
/// Pure, so the rule is tested without a machine to run it on. A `tmpfs`
/// under `/run` that is not `/run/media` is hidden by mount point rather than
/// by type, which is the case the design names.
pub fn is_pseudo(fs_type: &str, mount_point: &str) -> bool {
    if PSEUDO_FILESYSTEMS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(fs_type))
    {
        return true;
    }
    if PSEUDO_MOUNTS.iter().any(|dir| under(mount_point, dir)) {
        return true;
    }
    under(mount_point, "/run") && !under(mount_point, "/run/media")
}

/// Is `path` the directory `dir` or something inside it?
///
/// Text, not `Path::starts_with`: both sides are mount points as the kernel
/// reports them, and `/runtime` must not count as being under `/run`.
fn under(path: &str, dir: &str) -> bool {
    path == dir || path.strip_prefix(dir).is_some_and(|r| r.starts_with('/'))
}

/// Every mounted filesystem worth showing, freshly read.
///
/// Reads `/proc/mounts` through `sysinfo` on every call, deliberately: the
/// popup refreshes on open and the figures must be current. The
/// panel top border keeps its own cached path through `crate::ui::volume`.
pub fn enumerate(show_all: bool) -> Vec<Device> {
    let labels = labels();
    let raw = sysinfo::Disks::new_with_refreshed_list()
        .list()
        .iter()
        .map(|disk| {
            let mount_point = disk.mount_point().to_string_lossy().into_owned();
            let node = disk
                .name()
                .to_string_lossy()
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_string();
            let label = labels
                .get(&node)
                .filter(|label| !label.is_empty())
                .cloned()
                .unwrap_or_else(|| mount_point.clone());
            Device {
                mount_point,
                label,
                fs_type: disk.file_system().to_string_lossy().into_owned(),
                free: disk.available_space(),
                total: disk.total_space(),
                removable: disk.is_removable(),
                read_only: disk.is_read_only(),
            }
        })
        .collect();
    filter(raw, show_all)
}

/// [`enumerate`]'s pure half: filter and sort a list somebody else read.
///
/// Sorted by mount point, so `/` is first and the order is stable between two
/// refreshes a second apart - a popup whose rows reorder under the cursor is
/// unusable. The filesystem type breaks a tie, because two things can be
/// mounted on one directory and the order still has to be total.
pub fn filter(raw: Vec<Device>, show_all: bool) -> Vec<Device> {
    let mut out: Vec<Device> = raw
        .into_iter()
        .filter(|d| show_all || !is_pseudo(&d.fs_type, &d.mount_point))
        .collect();
    out.sort_by(|a, b| {
        a.mount_point
            .cmp(&b.mount_point)
            .then_with(|| a.fs_type.cmp(&b.fs_type))
    });
    out
}

/// The filesystem labels udev published, device node to label.
///
/// Read from `/dev/disk/by-label/`, whose entries are symlinks named for the
/// label and pointing at the device node. In process and with no dependency;
/// the design asks the popup for a "volume label" and this is the only
/// place Linux publishes one. An empty map on any error, because a picker
/// with mount points and no labels still works.
///
/// The key is the node's last component (`sda1`, `dm-0`), which is what
/// [`enumerate`] has from `sysinfo`. udev escapes a label's spaces and
/// non-ASCII bytes as `\x20`; [`unescape`] puts them back, so a stick labelled
/// `MY BACKUP` reads as itself rather than as `MY\x20BACKUP`.
pub fn labels() -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(dir) = std::fs::read_dir(BY_LABEL) else {
        return out;
    };
    for entry in dir.flatten() {
        let label = unescape(&entry.file_name().to_string_lossy());
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
        let Some(node) = target.file_name() else {
            continue;
        };
        out.insert(node.to_string_lossy().into_owned(), label);
    }
    out
}

/// Undo udev's `\x20` escaping of a label.
///
/// A trailing or malformed escape is left as the literal text it is: this is a
/// display string, and mangling it further would hide the oddity rather than
/// show it.
fn unescape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(at) = rest.find("\\x") {
        out.push_str(&rest[..at]);
        let after = &rest[at.saturating_add(2)..];
        let hex = after.get(..2).unwrap_or_default();
        match u8::from_str_radix(hex, 16) {
            Ok(byte) if byte.is_ascii() => {
                out.push(char::from(byte));
                rest = after.get(2..).unwrap_or_default();
            }
            // Not an escape after all, or a byte that is not a character on
            // its own: keep the text and step past the backslash.
            Ok(_) | Err(_) => {
                out.push_str("\\x");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `1.0T of 3.6T free`, or `1.0T free` when the total is unknown.
///
/// The same shortening ladder as the panel's volume line
/// (`crate::ui::volume::lines`), so one program does not have two ways of
/// writing a byte count.
pub fn free_of_total(device: &Device) -> String {
    if device.total == 0 {
        format!("{} free", human(device.free))
    } else {
        format!("{} of {} free", human(device.free), human(device.total))
    }
}

/// The markers the design puts at the end of a row: removable, read-only.
///
/// Empty for an ordinary fixed disk, which is most of them, so the field
/// costs nothing when it says nothing.
fn markers(device: &Device, ascii: bool) -> String {
    let mut out = String::new();
    if device.removable {
        out.push_str(if ascii { REMOVABLE_ASCII } else { REMOVABLE });
    }
    if device.read_only {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(READ_ONLY);
    }
    out
}

/// One popup row, fitted to `width` ("A row is one line").
///
/// Mount point, label, filesystem type, free of total, and the removable
/// marker. Fields are dropped from the right as the width shrinks, whole
/// rather than truncated, which is the rule for the volume line
/// applied to the same figures.
///
/// The mount point is never dropped: it is what the row means, and it is
/// cropped only when it alone is wider than the popup.
pub fn row(device: &Device, width: usize, ascii: bool) -> String {
    let free = free_of_total(device);
    let markers = markers(device, ascii);
    // A label that is only the mount point again is not a second field
    // (`enumerate` falls back to the mount point when udev published no
    // label), so it is left out rather than printed twice.
    let label: &str = if device.label == device.mount_point {
        ""
    } else {
        &device.label
    };
    let ladder: [&[&str]; 5] = [
        &[&device.mount_point, label, &device.fs_type, &free, &markers],
        &[&device.mount_point, label, &device.fs_type, &free],
        &[&device.mount_point, label, &free],
        &[&device.mount_point, &free],
        &[&device.mount_point],
    ];
    for fields in ladder {
        let line = join(fields);
        if text::width(&line) <= width {
            return line;
        }
    }
    text::truncate(&device.mount_point, width, Crop::End, ellipsis(ascii))
}

/// Join the fields of a row, dropping the ones that are empty so a missing
/// label does not leave a double gap.
fn join(fields: &[&str]) -> String {
    fields
        .iter()
        .filter(|field| !field.is_empty())
        .copied()
        .collect::<Vec<&str>>()
        .join(GAP)
}

/// The crop marker, which is `crate::ui::dialog`'s.
const fn ellipsis(ascii: bool) -> &'static str {
    if ascii { "..." } else { "\u{2026}" }
}

/// What quick search matches a device row against.
///
/// The mount point **with any leading `/` stripped**, so that the design's
/// own example works under the default `panel.quick_search = "prefix"`:
/// "typing `us` jumps to `/usr`", and `us` is not a prefix of `/usr`.
///
/// The root is the one mount point that is nothing but its leading slash, and
/// it keeps it: an empty key could never be typed, and `/` is what a user
/// would type to reach it.
pub fn search_key(mount_point: &str) -> &str {
    match mount_point.strip_prefix('/') {
        Some("") | None => mount_point,
        Some(rest) => rest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(mount: &str, fs: &str) -> Device {
        Device {
            mount_point: mount.to_string(),
            label: mount.to_string(),
            fs_type: fs.to_string(),
            free: 1_100_000_000_000,
            total: 4_000_000_000_000,
            removable: false,
            read_only: false,
        }
    }

    #[test]
    fn a_pseudo_filesystem_is_hidden_by_type() {
        assert!(is_pseudo("proc", "/proc"));
        assert!(is_pseudo("squashfs", "/snap/core/1"));
        assert!(is_pseudo("overlay", "/var/lib/docker/overlay2/x/merged"));
        assert!(!is_pseudo("ext4", "/home"));
        assert!(!is_pseudo("vfat", "/boot/efi"));
    }

    #[test]
    // the design names "tmpfs under /run" and udisks2 mounts removable
    // volumes under /run/media, so the two have to be told apart by mount
    // point rather than by type.
    fn a_tmpfs_under_run_is_hidden_but_run_media_is_not() {
        assert!(is_pseudo("tmpfs", "/run/user/1000"));
        assert!(is_pseudo("tmpfs", "/run"));
        assert!(!is_pseudo("vfat", "/run/media/thorin/USB"));
        // A directory whose name merely starts with the same letters is not
        // under it.
        assert!(!is_pseudo("ext4", "/runtime"));
    }

    #[test]
    fn the_filter_drops_pseudo_mounts_and_show_all_keeps_them() {
        let raw = vec![
            device("/proc", "proc"),
            device("/home", "ext4"),
            device("/", "btrfs"),
        ];
        let kept = filter(raw.clone(), false);
        assert_eq!(
            kept.iter()
                .map(|d| d.mount_point.as_str())
                .collect::<Vec<_>>(),
            vec!["/", "/home"]
        );
        let all = filter(raw, true);
        assert_eq!(all.len(), 3, "show_all lifts hcmd's own filter");
    }

    #[test]
    // a popup that reorders its rows under the
    // cursor while a stick is being plugged in is worse than one that does not
    // refresh at all.
    fn the_order_is_by_mount_point_and_stable() {
        let raw = vec![
            device("/home", "ext4"),
            device("/", "btrfs"),
            device("/boot", "vfat"),
        ];
        let once = filter(raw.clone(), false);
        let again = filter(raw, false);
        assert_eq!(once, again);
        assert_eq!(
            once.iter()
                .map(|d| d.mount_point.as_str())
                .collect::<Vec<_>>(),
            vec!["/", "/boot", "/home"]
        );
    }

    #[test]
    fn free_of_total_says_free_alone_when_the_total_is_unknown() {
        let mut d = device("/", "ext4");
        assert_eq!(free_of_total(&d), "1.0T of 3.6T free");
        d.total = 0;
        assert_eq!(free_of_total(&d), "1.0T free");
    }

    #[test]
    fn a_row_shows_every_field_spec_17_1_names() {
        let mut d = device("/run/media/thorin/USB", "vfat");
        d.label = "BACKUP".to_string();
        d.removable = true;
        let line = row(&d, 80, true);
        assert!(line.contains("/run/media/thorin/USB"), "{line}");
        assert!(line.contains("BACKUP"), "{line}");
        assert!(line.contains("vfat"), "{line}");
        assert!(line.contains("free"), "{line}");
        assert!(line.contains(REMOVABLE_ASCII), "{line}");
    }

    #[test]
    // the rule for the volume line: a field is given up whole rather
    // than cropped to a stump, and the mount point is the last to go.
    fn fields_are_surrendered_from_the_right_and_the_mount_point_never_is() {
        let mut d = device("/home", "ext4");
        d.label = "data".to_string();
        d.read_only = true;
        for width in 1..=80 {
            let line = row(&d, width, true);
            assert!(
                text::width(&line) <= width,
                "width {width}: {line:?} overflows"
            );
            if width >= text::width("/home") {
                assert!(line.starts_with("/home"), "width {width}: {line:?}");
            }
        }
        assert_eq!(row(&d, 5, true), "/home");
    }

    #[test]
    // A label that is only the mount point again is `enumerate`'s fallback,
    // not something udev published, and printing it twice reads as a bug.
    fn a_label_equal_to_the_mount_point_is_not_repeated() {
        let d = device("/home", "ext4");
        let line = row(&d, 80, true);
        assert_eq!(line.matches("/home").count(), 1, "{line}");
    }

    #[test]
    // the own example: "typing `us` jumps to `/usr`", under the
    // default `panel.quick_search = "prefix"` (the design I4).
    fn the_search_key_drops_the_leading_slash() {
        assert_eq!(search_key("/usr"), "usr");
        assert_eq!(search_key("/run/media/thorin/USB"), "run/media/thorin/USB");
        assert_eq!(search_key("/"), "/", "the root stays typeable");
    }

    #[test]
    fn udev_escapes_are_undone() {
        assert_eq!(unescape("MY\\x20BACKUP"), "MY BACKUP");
        assert_eq!(unescape("plain"), "plain");
        assert_eq!(unescape("trailing\\x"), "trailing\\x");
        assert_eq!(unescape("\\xzz"), "\\xzz");
    }

    #[test]
    // CI has no removable volume, so the only
    // honest assertion about the machine's own mount table is that reading it
    // works. This is what `crate::ui::volume` already does, for the same
    // reason.
    fn enumerating_the_real_machine_does_not_panic() {
        let _ = enumerate(false);
        let _ = enumerate(true);
        let _ = labels();
    }
}
