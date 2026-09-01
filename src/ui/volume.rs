//! The volume line.
//!
//! Rendered as each panel block's **top border title**: the device, its label,
//! and free of total space -
//! `d [dev]  988G of 3.6T free (73% used)`.
//!
//! This is display only. `Alt+F1`/`Alt+F2` open the device picker, which
//! the design puts in v0.7 and which is out of scope here.
//!
//! `sysinfo` enumerates mounts. The enumeration is cached and refreshed at most
//! once every [`REFRESH`], because it is a `/proc` walk plus a `statvfs` per
//! mount and the renderer runs on every frame.

use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// How stale the free-space figure is allowed to get.
const REFRESH: Duration = Duration::from_secs(5);

/// One mount, as the volume line needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// The device, without a `/dev/` prefix: `nvme0n1p2`.
    ///
    /// Kept because the device picker wants it, and **deliberately not shown in
    /// the panel's top border**: on Linux it is the least informative field
    /// available. A LUKS or LVM mapper is routinely named `root`, which reads
    /// as the superuser and means nothing about where you are; the reference
    /// screenshot's `d [dev]` is a Windows drive letter plus a volume label,
    /// and neither has a Linux counterpart worth borrowing.
    pub device: String,
    /// The filesystem label where there is one, else the mount point - the
    /// field that actually says *which filesystem* this is, which is what makes
    /// two panels comparable and what decides whether `F6` is a rename or a
    /// copy.
    pub label: String,
    /// The mount point, used to pick the longest match.
    pub mount_point: String,
    /// Free bytes.
    pub free: u64,
    /// Total bytes.
    pub total: u64,
}

/// The volume holding `path`: the mount whose mount point is the longest prefix
/// of it.
pub fn for_path(path: &Path) -> Option<Volume> {
    let mounts = mounts();
    best_match(&mounts, path)
}

/// The longest mount point that is a prefix of `path`.
///
/// Split out from [`for_path`] so it is testable without touching `/proc`.
pub fn best_match(mounts: &[Volume], path: &Path) -> Option<Volume> {
    mounts
        .iter()
        .filter(|v| path.starts_with(&v.mount_point))
        .max_by_key(|v| v.mount_point.len())
        .cloned()
}

/// The longest volume line for a mount: human-readable free-of-total, with the
/// percentage in use.
///
/// The condensed top border has room for the figures in the same shortened
/// units the rest of the program uses (`847G`, not a nine-digit kilobyte
/// count), so the `% used` fits beside them and answers the question the raw
/// numbers only imply.
pub fn line(volume: &Volume) -> String {
    let used = volume.total.saturating_sub(volume.free);
    let percent = if volume.total > 0 {
        // used <= total, so the ratio is 0.0..=1.0 and the product 0..=100.
        #[expect(
            clippy::cast_precision_loss,
            reason = "a percentage does not need byte-exact precision"
        )]
        let pct = (used as f64 / volume.total as f64 * 100.0).round();
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "pct is 0.0..=100.0"
        )]
        let pct = pct as u64;
        pct
    } else {
        0
    };
    format!(
        "[{}]  {} of {} free ({percent}% used)",
        volume.label,
        human(volume.free),
        human(volume.total),
    )
}

/// Progressively shorter renderings of the same volume, longest first.
///
/// The top border carries the path *and* the volume line on one row, and the
/// path has priority - but free space vanishing entirely on a merely narrowish
/// panel is a poor trade, so the caller takes the longest of these that fits
/// rather than choosing between all and nothing.
pub fn lines(volume: &Volume) -> [String; 4] {
    [
        line(volume),
        format!(
            "[{}] {} of {} free",
            volume.label,
            human(volume.free),
            human(volume.total)
        ),
        format!("{} of {} free", human(volume.free), human(volume.total)),
        format!("{} free", human(volume.free)),
    ]
}

/// A compact byte count: `847G`, `12.4G`, `512M`. Binary units, one decimal
/// below ten, so the field never grows past five cells.
///
/// Public because the device picker prints the same figures in the
/// same shortening ladder (`crate::devices::free_of_total`), and one program
/// may not have two ways of writing a byte count.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "K", "M", "G", "T", "P"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    let suffix = UNITS.get(unit).copied().unwrap_or("B");
    if unit == 0 || value >= 10.0 {
        format!("{}{suffix}", value.round() as u64)
    } else {
        format!("{value:.1}{suffix}")
    }
}

/// What the top border says when the path is not on a local mount, or when
/// `sysinfo` could not enumerate anything (an unprivileged container, a `/proc`
/// that is not mounted). Never an error: the panel still works.
pub fn unknown_line(title: &str) -> String {
    format!("{title} [_none_]")
}

/// The cached mount list.
static MOUNTS: LazyLock<Mutex<Cache>> = LazyLock::new(|| {
    Mutex::new(Cache {
        at: None,
        volumes: Vec::new(),
    })
});

struct Cache {
    at: Option<Instant>,
    volumes: Vec<Volume>,
}

/// Every mount, refreshed at most once per [`REFRESH`].
///
/// A poisoned lock - only reachable if a panic happened inside this module,
/// which nothing here can do - degrades to a fresh enumeration rather than
/// propagating.
pub fn mounts() -> Vec<Volume> {
    let now = Instant::now();
    let Ok(mut cache) = MOUNTS.lock() else {
        return enumerate();
    };
    let stale = cache
        .at
        .is_none_or(|at| now.saturating_duration_since(at) >= REFRESH);
    if stale {
        cache.volumes = enumerate();
        cache.at = Some(now);
    }
    cache.volumes.clone()
}

fn enumerate() -> Vec<Volume> {
    sysinfo::Disks::new_with_refreshed_list()
        .list()
        .iter()
        .map(|disk| {
            let device = disk.name().to_string_lossy().into_owned();
            let device = device
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .map_or_else(|| device.clone(), str::to_string);
            let mount_point = disk.mount_point().to_string_lossy().into_owned();
            let label = if mount_point.is_empty() {
                "_none_".to_string()
            } else {
                mount_point.clone()
            };
            Volume {
                device,
                label,
                mount_point,
                free: disk.available_space(),
                total: disk.total_space(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(mount: &str) -> Volume {
        Volume {
            device: "sda1".to_string(),
            label: mount.to_string(),
            mount_point: mount.to_string(),
            free: 1_062_892_164 * 1024,
            total: 3_907_000_316 * 1024,
        }
    }

    #[test]
    fn the_longest_mount_point_wins() {
        let mounts = vec![v("/"), v("/home"), v("/home/thorin/data")];
        let got = best_match(&mounts, Path::new("/home/thorin/data/x"));
        assert_eq!(got.map(|m| m.mount_point), Some("/home/thorin/data".into()));
        let got = best_match(&mounts, Path::new("/home/other"));
        assert_eq!(got.map(|m| m.mount_point), Some("/home".into()));
        let got = best_match(&mounts, Path::new("/var/log"));
        assert_eq!(got.map(|m| m.mount_point), Some("/".into()));
    }

    #[test]
    fn no_mount_matches_when_the_list_is_empty() {
        assert_eq!(best_match(&[], Path::new("/")), None);
    }

    #[test]
    // The device name is deliberately absent: the design drops it because a
    // LUKS or LVM mapper called `root` says nothing about where you are.
    fn the_line_is_human_readable_with_a_used_percentage() {
        let line = line(&v("/"));
        assert!(line.starts_with("[/]  "), "{line}");
        assert!(line.contains(" free ("), "{line}");
        assert!(line.contains("% used)"), "{line}");
        assert!(
            !line.contains(" k of "),
            "human units, not raw kilobytes: {line}"
        );
    }

    #[test]
    fn enumerating_the_real_machine_does_not_panic() {
        let _ = mounts();
        let _ = for_path(Path::new("/"));
    }
}
