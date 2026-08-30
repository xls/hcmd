//! Turning an [`Entry`] into the text of one panel row.
//!
//! Three formatting decisions are read off the reference panel rather than
//! assumed, and they are the defaults:
//!
//! * **Sizes are exact byte counts with thousands separators** - `362,333`, not
//!   `362 K`. `panel.human_sizes = true` switches to human-readable.
//! * **Directories are bracketed** - `[bin]`, `[..]` - so they are
//!   distinguishable from files without colour, which is what survives a
//!   16-colour terminal and colour-blindness.
//! * **Attributes** are the permission string on Unix - `drwxr-xr-x` - with
//!   `panel.attr_style = "dos"` offering the four-character `-a--` form for
//!   muscle memory, hidden mapped from the leading dot.

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local};

use crate::config::{AttrStyle, PanelConfig};
use crate::ops::SizeCache;
use crate::panel::columns::{Allocation, SEPARATOR};
use crate::panel::text::{self, Align, Crop};
use crate::panel::{ColumnId, SortState, Tab};
use crate::vfs::{Entry, EntryKind};

/// What a directory shows in the `size` column.
pub const DIR_MARKER: &str = "<DIR>";

/// One rendered cell: text already cropped and padded to exactly `width` cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// Which column this is.
    pub id: ColumnId,
    /// The padded text. Its display width is exactly [`Cell::width`].
    pub text: String,
    /// The allocated width.
    pub width: usize,
    /// How the text sits in the cell.
    pub align: Align,
    /// True when the underlying value did not fit and was cropped. The panel
    /// status line shows the full name of the entry under the cursor when this
    /// is set on its `name` cell.
    pub cropped: bool,
}

/// Which way a column's content is aligned (`size` is
/// right-aligned; everything else reads better flush left).
pub const fn align_of(id: ColumnId) -> Align {
    match id {
        ColumnId::Size => Align::Right,
        _ => Align::Left,
    }
}

// --------------------------------------------------------------- values -----

/// Group a byte count into thousands: `362333` becomes `362,333`.
pub fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len().saturating_add(digits.len() / 3));
    let leading = match digits.len() % 3 {
        0 => 3,
        n => n,
    };
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && i >= leading && (i.saturating_sub(leading)) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `panel.human_sizes = true`: `1.2 M` instead of `1,234,567`.
///
/// Binary units, one decimal below ten, none above, which is what keeps the
/// column narrow enough to be worth switching to.
pub fn human_size(value: u64) -> String {
    const UNITS: [&str; 6] = ["K", "M", "G", "T", "P", "E"];
    if value < 1024 {
        return value.to_string();
    }
    let mut scaled = value as f64 / 1024.0;
    let mut unit = UNITS.first().copied().unwrap_or("K");
    for next in UNITS.iter().skip(1) {
        if scaled < 1024.0 {
            break;
        }
        scaled /= 1024.0;
        unit = *next;
    }
    if scaled < 10.0 {
        format!("{scaled:.1} {unit}")
    } else {
        format!("{} {unit}", scaled.round() as u64)
    }
}

/// The `size` column's text: `<DIR>` for a directory, otherwise a byte count
/// formatted according to `panel.human_sizes` and
/// `panel.thousands_separator`.
pub fn size_text(entry: &Entry, cfg: &PanelConfig) -> String {
    size_text_with(entry, cfg, None)
}

/// The `size` column's text, with a directory's computed size when one is
/// known.
///
/// > `Space` … on a directory, also walk it to full depth and **show its
/// > size**.
///
/// That is what `sized` is for: pass `app.jobs.sizes.get(&tab.path.join(&name)).
/// map(|s| s.bytes)` and a walked directory shows its byte count in place of
/// `<DIR>`, formatted exactly as a file's is, so the two never disagree.
/// `None` - every directory that has not been walked, and every file - is what
/// [`size_text`] renders on its own.
pub fn size_text_with(entry: &Entry, cfg: &PanelConfig, sized: Option<u64>) -> String {
    let bytes = if entry.is_parent {
        return DIR_MARKER.to_string();
    } else if entry.is_dir() {
        match sized {
            Some(bytes) => bytes,
            None => return DIR_MARKER.to_string(),
        }
    } else {
        entry.size
    };
    if cfg.human_sizes {
        human_size(bytes)
    } else if cfg.thousands_separator {
        thousands(bytes)
    } else {
        bytes.to_string()
    }
}

/// The `size` column's text, narrowed to the width the column actually got.
///
/// A byte count is the one cell whose exact digits are worth more than its
/// shape until it stops fitting, at which point they are worth nothing: an
/// end-cropped `1,073,741,8...` reads as a smaller number than it is, which is
/// worse than no number. So the plain or grouped form is used while it fits,
/// and a file too large for the column falls back to `1.0 G`, which is what a
/// reader wanted from a narrow column anyway.
///
/// Never widens: with `panel.human_sizes` on, the human form is already the
/// first thing tried and this changes nothing.
pub fn size_text_fitting(
    entry: &Entry,
    cfg: &PanelConfig,
    sized: Option<u64>,
    width: usize,
) -> String {
    let exact = size_text_with(entry, cfg, sized);
    if crate::ui::text::width(&exact) <= width || cfg.human_sizes {
        return exact;
    }
    // `<DIR>` and an empty cell are not numbers and have no shorter form.
    let Some(bytes) = size_bytes(entry, sized) else {
        return exact;
    };
    human_size(bytes)
}

/// The number a size cell is about, or `None` when the cell is not a number.
fn size_bytes(entry: &Entry, sized: Option<u64>) -> Option<u64> {
    if entry.is_parent {
        return None;
    }
    if entry.is_dir() {
        return sized;
    }
    Some(entry.size)
}

/// The `date` column's text, in the local time zone, using
/// `panel.date_format`.
///
/// A format string that `chrono` cannot render falls back to the built-in
/// `"%Y-%m-%d %H:%M"` and then to the empty string, because `panel.date_format`
/// is user input and a bad one must not take the panel down.
///
/// An mtime that `chrono` cannot represent renders as an empty cell, the same
/// as a missing one. Timestamps are unvalidated I/O data - `touch -d
/// @9999999999999` is enough to produce one, and so is an archive with a bogus
/// header - so the conversion goes through the fallible
/// [`DateTime::from_timestamp`] rather than `SystemTime::into`, whose chrono
/// impl ends in an `unwrap`.
pub fn date_text(entry: &Entry, cfg: &PanelConfig) -> String {
    let Some(mtime) = entry.mtime else {
        return String::new();
    };
    let Some(local) = to_local(mtime) else {
        return String::new();
    };
    let mut out = String::new();
    if local.format(&cfg.date_format).write_to(&mut out).is_ok() {
        return out;
    }
    out.clear();
    if local.format("%Y-%m-%d %H:%M").write_to(&mut out).is_ok() {
        return out;
    }
    String::new()
}

/// Convert a [`SystemTime`] to local time, or `None` when it falls outside the
/// range `chrono` can represent.
///
/// Both directions are handled: `duration_since` returns `Err` for a pre-epoch
/// timestamp, and the epoch-relative seconds are carried as a signed value so a
/// 1970-or-earlier mtime is a date rather than a wrap-around.
fn to_local(mtime: SystemTime) -> Option<DateTime<Local>> {
    let (secs, nanos) = match mtime.duration_since(UNIX_EPOCH) {
        Ok(dur) => (i64::try_from(dur.as_secs()).ok()?, dur.subsec_nanos()),
        Err(err) => {
            let dur = err.duration();
            let secs = i64::try_from(dur.as_secs()).ok()?.checked_neg()?;
            match dur.subsec_nanos() {
                0 => (secs, 0),
                nanos => (secs.checked_sub(1)?, 1_000_000_000 - nanos),
            }
        }
    };
    DateTime::from_timestamp(secs, nanos).map(|utc| utc.with_timezone(&Local))
}

/// The ten-character Unix permission string: `drwxr-xr-x`.
pub fn unix_attr(entry: &Entry) -> String {
    let type_char = match entry.kind {
        EntryKind::Dir => 'd',
        EntryKind::Symlink { .. } => 'l',
        EntryKind::File => '-',
        EntryKind::Other => match entry.mode & 0o170_000 {
            0o140_000 => 's',
            0o010_000 => 'p',
            0o020_000 => 'c',
            0o060_000 => 'b',
            _ => '?',
        },
    };
    let mut out = String::with_capacity(10);
    out.push(type_char);
    // Owner, group, other; setuid/setgid/sticky fold into the execute slot the
    // way `ls` renders them.
    let triples = [
        (0o400, 0o200, 0o100, 0o4000, 's'),
        (0o040, 0o020, 0o010, 0o2000, 's'),
        (0o004, 0o002, 0o001, 0o1000, 't'),
    ];
    for (r, w, x, special, special_char) in triples {
        out.push(if entry.mode & r != 0 { 'r' } else { '-' });
        out.push(if entry.mode & w != 0 { 'w' } else { '-' });
        out.push(match (entry.mode & x != 0, entry.mode & special != 0) {
            (true, true) => special_char,
            (true, false) => 'x',
            (false, true) => special_char.to_ascii_uppercase(),
            (false, false) => '-',
        });
    }
    out
}

/// The four-character DOS approximation: `r a h s`.
///
/// * `r` - read-only: the owner has no write bit.
/// * `a` - archive: set for regular files, which is what Windows does to
///   everything that has been written since the last backup.
/// * `h` - hidden: the leading dot, which is the Unix convention the DOS flag
///   maps onto.
/// * `s` - system: never set on Unix; the slot is kept so the column stays four
///   characters wide and lines up.
pub fn dos_attr(entry: &Entry) -> String {
    if entry.is_parent {
        return "----".to_string();
    }
    let read_only = entry.mode != 0 && entry.mode & 0o200 == 0;
    // Everything that is not a directory carries the archive bit, which is the
    // closest DOS has to "ordinary file" - the design calls this style a
    // four-character *approximation*. Keying it on `EntryKind::File` alone left
    // symlinks, fifos and devices showing `----`, which reads as "directory".
    let archive = !entry.is_dir();
    let mut out = String::with_capacity(4);
    out.push(if read_only { 'r' } else { '-' });
    out.push(if archive { 'a' } else { '-' });
    out.push(if entry.is_hidden { 'h' } else { '-' });
    out.push('-');
    out
}

/// The `attr` column's text, honouring `panel.attr_style`.
pub fn attr_text(entry: &Entry, style: AttrStyle) -> String {
    match style {
        AttrStyle::Unix => unix_attr(entry),
        AttrStyle::Dos => dos_attr(entry),
    }
}

/// The `perms_octal` column's text: `0644`.
pub fn perms_octal_text(entry: &Entry) -> String {
    format!("{:04o}", entry.mode & 0o7777)
}

/// The `name` column's text.
///
/// The extension is split off only when the `ext` column is rendered; otherwise
/// it stays in the name, which is what makes the middle-crop rule of the design
/// worth having. Directories are bracketed when `panel.dir_brackets` is set,
/// `[..]` included.
pub fn name_text(entry: &Entry, cfg: &PanelConfig, ext_visible: bool) -> String {
    let base = if ext_visible {
        entry.split_name().0
    } else {
        entry.name.as_str()
    };
    if cfg.dir_brackets && (entry.is_dir() || entry.is_parent) {
        format!("[{base}]")
    } else {
        base.to_string()
    }
}

/// [`cell_text`] with a directory's computed size, when one is known.
///
/// `Space` on a directory "walks it to full depth and **shows
/// its size**" - in the `size` column, not only in the status line. `sized` is
/// `app.jobs.sizes.get(&tab.path.join(&entry.name)).map(|s| s.bytes)`; `None` is
/// every unsized directory and every file, which is what [`cell_text`] renders
/// on its own - this is that function plus the one thing it cannot know.
pub fn cell_text_with(
    entry: &Entry,
    id: ColumnId,
    cfg: &PanelConfig,
    ext_visible: bool,
    sized: Option<u64>,
    local_ids: bool,
) -> String {
    if id == ColumnId::Size {
        return size_text_with(entry, cfg, sized);
    }
    cell_text(entry, id, cfg, ext_visible, local_ids)
}

/// The unpadded text of one column for one entry.
///
/// `local_ids` says whether `uid` and `gid` may be resolved against **this
/// machine's** passwd database. For a row that did not come from this
/// machine - an archive member, a file on `nas.local` - they may not: uid 1000
/// there is not the person sitting here, and rendering their name is a wrong
/// answer that looks like a right one.
/// The rule is about **provenance, not protocol**, which is the same
/// distinction [`crate::vfs::untrusted_mode`] already draws.
pub fn cell_text(
    entry: &Entry,
    id: ColumnId,
    cfg: &PanelConfig,
    ext_visible: bool,
    local_ids: bool,
) -> String {
    match id {
        ColumnId::Name => name_text(entry, cfg, ext_visible),
        ColumnId::Ext => {
            if entry.is_dir() || entry.is_parent {
                String::new()
            } else {
                entry.extension().to_string()
            }
        }
        ColumnId::Size => size_text(entry, cfg),
        ColumnId::Date => date_text(entry, cfg),
        ColumnId::Attr => attr_text(entry, cfg.attr_style),
        // uid/gid resolved to names, cached. An id with no entry
        // in `/etc/passwd` falls back to the number - and a row from somewhere
        // that is not this machine falls back to it too, because this
        // machine's passwd database says nothing about that row's owner.
        ColumnId::Owner if local_ids => crate::vfs::owner_name(entry.uid),
        ColumnId::Group if local_ids => crate::vfs::group_name(entry.gid),
        ColumnId::Owner => entry.uid.to_string(),
        ColumnId::Group => entry.gid.to_string(),
        ColumnId::PermsOctal => perms_octal_text(entry),
    }
}

// ---------------------------------------------------------------- rows ------

/// Render one entry into cells for the given allocation.
///
/// Every cell comes back padded to exactly its allocated width, so a caller can
/// concatenate them with [`SEPARATOR`] and get an aligned row, or style each one
/// separately and get the same geometry.
pub fn render_row(
    entry: &Entry,
    alloc: &Allocation,
    cfg: &PanelConfig,
    ascii: bool,
    local_ids: bool,
) -> Vec<Cell> {
    let ext_visible = alloc.is_visible(ColumnId::Ext);
    alloc
        .columns()
        .iter()
        .map(|col| {
            let raw = cell_text(entry, col.id, cfg, ext_visible, local_ids);
            let crop = if col.id == ColumnId::Name {
                alloc.crop()
            } else {
                Crop::End
            };
            let align = align_of(col.id);
            Cell {
                id: col.id,
                cropped: text::is_cropped(&raw, col.width),
                text: text::fit(&raw, col.width, crop, align, ascii),
                width: col.width,
                align,
            }
        })
        .collect()
}

/// One entry as a single line, cells joined by [`SEPARATOR`].
///
/// This is what the tests assert on and what a caller that does not need
/// per-cell styling can render directly.
pub fn row_text(
    entry: &Entry,
    alloc: &Allocation,
    cfg: &PanelConfig,
    ascii: bool,
    local_ids: bool,
) -> String {
    join(&render_row(entry, alloc, cfg, ascii, local_ids))
}

/// The column header row, with the sort arrow prefixed to the sorted column's
/// header text as Total Commander does it - `▲Ext`, `▲Name`.
///
/// A sorted column that is hidden contributes no arrow here, which is exactly
/// why the status-line tag from [`SortState::indicator`] exists as well.
pub fn header_row(alloc: &Allocation, sort: SortState, ascii: bool) -> Vec<Cell> {
    alloc
        .columns()
        .iter()
        .map(|col| {
            let raw = sort.header_text(col.id, ascii);
            let align = align_of(col.id);
            Cell {
                id: col.id,
                cropped: text::is_cropped(&raw, col.width),
                text: text::fit(&raw, col.width, Crop::End, align, ascii),
                width: col.width,
                align,
            }
        })
        .collect()
}

/// The header as a single line.
pub fn header_text(alloc: &Allocation, sort: SortState, ascii: bool) -> String {
    join(&header_row(alloc, sort, ascii))
}

/// Join rendered cells with the one-space column separator.
pub fn join(cells: &[Cell]) -> String {
    let mut out = String::new();
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            out.push_str(SEPARATOR);
        }
        out.push_str(&cell.text);
    }
    out
}

// -------------------------------------------------------- status line -------

/// The counts the panel status line reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Bytes in marked files.
    pub marked_bytes: u64,
    /// Bytes in all files.
    pub total_bytes: u64,
    /// Marked files.
    pub marked_files: usize,
    /// Files, excluding directories and `..`.
    pub total_files: usize,
    /// Marked directories.
    pub marked_dirs: usize,
    /// Directories, excluding `..`.
    pub total_dirs: usize,
    /// Marked directories whose size has not been computed.
    ///
    /// Non-zero is exactly when the selection's size is a **lower bound** and
    /// the line renders `≥`. It is not the same as `marked_dirs`: `Space` and
    /// `Ctrl+L` resolve directories one at a time, so a selection can hold five
    /// marked directories of which three are known.
    pub unsized_dirs: usize,
}

/// The panel status line's counts.
///
/// Two forms. With nothing marked, the directory's own totals:
///
/// ```text
/// 17,250 k in 43 files, 1 dir
/// ```
///
/// With something marked, the selection against that total, and its size:
///
/// ```text
/// 1 of 10 selected · 2,180 k
/// ```
///
/// # The `≥`
///
/// the design sizes a directory only on `Space` (or `Ctrl+L` over the whole
/// selection) - the bulk marking keys deliberately do not walk trees. A marked
/// directory therefore contributes to the *counts* but not to the *size* until
/// something has sized it, which makes the selection's size a **lower bound**:
///
/// ```text
/// 3 of 10 selected · ≥ 2,180 k
/// ```
///
/// The marker is `>=` under `ui.ascii_borders`. `Space` on a directory sizes
/// that one directory and `Ctrl+L` resolves the whole selection; each result
/// lands in [`crate::ops::SizeCache`], and the `≥` disappears once every
/// marked directory is in it. Until then the bound is the honest rendering of
/// what is actually known, which is exactly why the marker rather than a total
/// that merely looks computed.
///
/// The sort indicator is appended by the renderer at the right end of the same
/// line, and only when the sorted column is not drawn.
pub fn status_text(tab: &Tab, cfg: &PanelConfig, ascii: bool, sizes: &SizeCache) -> String {
    let c = tab.counts_with(sizes);
    let marked = c.marked_files.saturating_add(c.marked_dirs);

    if marked == 0 {
        let total = c.total_files.saturating_add(c.total_dirs);
        let _ = total;
        let mut out = String::new();
        let _ = write!(
            out,
            "{} in {} file{}, {} dir{}",
            status_size(c.total_bytes, cfg),
            c.total_files,
            if c.total_files == 1 { "" } else { "s" },
            c.total_dirs,
            if c.total_dirs == 1 { "" } else { "s" },
        );
        return out;
    }

    let total = c.total_files.saturating_add(c.total_dirs);
    // A marked directory that has not been walked contributes to the counts but
    // not to the size, so the total is a lower bound until every one of them
    // has.
    let bound = if c.unsized_dirs > 0 {
        if ascii { ">= " } else { "\u{2265} " }
    } else {
        ""
    };
    let sep = if ascii { "-" } else { "\u{b7}" };
    let mut out = String::new();
    let _ = write!(
        out,
        "{marked} of {total} selected {sep} {bound}{}",
        status_size(c.marked_bytes, cfg),
    );
    out
}

/// A byte count as the status line writes it.
///
/// Public because the quick view reports a directory's total, and
/// "a quick view and a status line write a byte count the same way" is a
/// promise one function can keep and two cannot.
///
/// Kilobytes with a `k` suffix - the form the diagram and the examples both
/// use - grouped according to `panel.thousands_separator`, or the
/// human-readable form when `panel.human_sizes` is on, so the status line and
/// the `size` column never disagree about what a byte count looks like.
pub fn status_size(bytes: u64, cfg: &PanelConfig) -> String {
    if cfg.human_sizes {
        return human_size(bytes);
    }
    let k = bytes.div_ceil(1024);
    let mut out = if cfg.thousands_separator {
        thousands(k)
    } else {
        k.to_string()
    };
    out.push_str(" k");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::ops::SizeCache;
    use crate::panel::columns::allocate;
    use std::time::{Duration, UNIX_EPOCH};

    /// No directory has been sized, which is the state every panel starts in.
    fn empty() -> SizeCache {
        SizeCache::new()
    }

    fn cfg() -> PanelConfig {
        Config::default().panel
    }

    #[test]
    fn sizes_are_exact_byte_counts_with_separators_by_default() {
        let cfg = cfg();
        let mut e = Entry::file("a.bin");
        e.size = 362_333;
        assert_eq!(size_text(&e, &cfg), "362,333");
        e.size = 1_062_892_164;
        assert_eq!(size_text(&e, &cfg), "1,062,892,164");
        e.size = 0;
        assert_eq!(size_text(&e, &cfg), "0");
        e.size = 999;
        assert_eq!(size_text(&e, &cfg), "999");
        e.size = 1000;
        assert_eq!(size_text(&e, &cfg), "1,000");
    }

    #[test]
    fn directories_render_the_dir_marker_not_a_byte_count() {
        let cfg = cfg();
        let mut d = Entry::dir("bin");
        d.size = 4096;
        assert_eq!(size_text(&d, &cfg), "<DIR>");
        assert_eq!(size_text(&Entry::parent_entry(), &cfg), "<DIR>");
        // once `Space` has walked it, the directory shows its
        // size in the same form a file's is shown in.
        assert_eq!(size_text_with(&d, &cfg, Some(362_333)), "362,333");
        assert_eq!(
            size_text_with(&Entry::parent_entry(), &cfg, Some(1)),
            "<DIR>",
            "`..` is never counted and never sized"
        );
    }

    #[test]
    fn human_sizes_are_opt_in() {
        let mut cfg = cfg();
        cfg.human_sizes = true;
        let mut e = Entry::file("a.bin");
        e.size = 362_333;
        assert_eq!(size_text(&e, &cfg), "354 K");
        e.size = 1024;
        assert_eq!(size_text(&e, &cfg), "1.0 K");
        e.size = 512;
        assert_eq!(size_text(&e, &cfg), "512");
    }

    #[test]
    fn directories_are_bracketed_so_colour_is_never_load_bearing() {
        let cfg = cfg();
        assert_eq!(name_text(&Entry::dir("bin"), &cfg, true), "[bin]");
        assert_eq!(name_text(&Entry::parent_entry(), &cfg, true), "[..]");
        assert_eq!(name_text(&Entry::file("a.rs"), &cfg, true), "a");
        assert_eq!(
            name_text(&Entry::file("a.rs"), &cfg, false),
            "a.rs",
            "with ext hidden the extension stays in the name"
        );
    }

    #[test]
    fn a_dotfile_has_no_extension() {
        let cfg = cfg();
        let e = Entry::file(".bashrc");
        assert_eq!(e.extension(), "");
        assert_eq!(name_text(&e, &cfg, true), ".bashrc");
        let e = Entry::file("a.tar.gz");
        assert_eq!(e.extension(), "gz");
        assert_eq!(name_text(&e, &cfg, true), "a.tar");
    }

    #[test]
    fn extension_case_is_preserved() {
        let e = Entry::file("PHOTO.JPG");
        assert_eq!(e.extension(), "JPG");
        let cfg = cfg();
        assert_eq!(cell_text(&e, ColumnId::Ext, &cfg, true, true), "JPG");
    }

    #[test]
    fn unix_attributes_are_ten_characters() {
        let mut d = Entry::dir("bin");
        d.mode = 0o040_755;
        assert_eq!(unix_attr(&d), "drwxr-xr-x");
        let mut f = Entry::file("a");
        f.mode = 0o100_644;
        assert_eq!(unix_attr(&f), "-rw-r--r--");
        assert_eq!(unix_attr(&f).chars().count(), 10);
        let mut l = Entry::file("link");
        l.kind = EntryKind::Symlink { to_dir: false };
        l.mode = 0o120_777;
        assert_eq!(unix_attr(&l), "lrwxrwxrwx");
    }

    #[test]
    fn dos_attributes_match_the_reference_screenshot() {
        // "[undermarquee]" - a writable directory - renders "----".
        let mut d = Entry::dir("undermarquee");
        d.mode = 0o040_755;
        assert_eq!(dos_attr(&d), "----");
        // "1 - BASE PANEL.3mf" - a writable file - renders "-a--".
        let mut f = Entry::file("1 - BASE PANEL.3mf");
        f.mode = 0o100_644;
        assert_eq!(dos_attr(&f), "-a--");
        // "[..]" renders "----".
        assert_eq!(dos_attr(&Entry::parent_entry()), "----");
        // A hidden read-only file.
        let mut h = Entry::file(".secret");
        h.mode = 0o100_444;
        assert_eq!(dos_attr(&h), "rah-");
    }

    #[test]
    fn octal_permissions_are_four_digits() {
        let mut f = Entry::file("a");
        f.mode = 0o100_644;
        assert_eq!(perms_octal_text(&f), "0644");
        f.mode = 0o100_000;
        assert_eq!(perms_octal_text(&f), "0000");
    }

    #[test]
    fn a_broken_date_format_yields_a_blank_cell_not_a_panic() {
        let mut cfg = cfg();
        cfg.date_format = "%".to_string();
        let mut e = Entry::file("a");
        e.mtime = Some(UNIX_EPOCH + Duration::from_secs(1_760_000_000));
        // Either the fallback format or an empty string; never a panic.
        let text = date_text(&e, &cfg);
        assert!(text.is_empty() || text.len() == 16, "{text:?}");
    }

    #[test]
    fn an_mtime_beyond_chronos_range_renders_an_empty_cell_not_a_panic() {
        // `touch -d @9999999999999` produces exactly this, and so does an
        // archive header with a bogus timestamp. Unvalidated I/O data must not
        // take the panel down.
        let cfg = cfg();
        for secs in [9_999_999_999_999_u64, 1 << 60] {
            let Some(when) = UNIX_EPOCH.checked_add(Duration::from_secs(secs)) else {
                continue;
            };
            let mut e = Entry::file("farfuture");
            e.mtime = Some(when);
            assert_eq!(date_text(&e, &cfg), "", "secs {secs}");
        }
    }

    #[test]
    fn an_mtime_far_before_the_epoch_renders_an_empty_cell_not_a_panic() {
        let cfg = cfg();
        for secs in [9_999_999_999_999_u64, 1 << 60] {
            let Some(when) = UNIX_EPOCH.checked_sub(Duration::from_secs(secs)) else {
                continue;
            };
            let mut e = Entry::file("farpast");
            e.mtime = Some(when);
            assert_eq!(date_text(&e, &cfg), "", "-{secs}");
        }
    }

    #[test]
    fn a_pre_epoch_mtime_inside_chronos_range_still_renders_a_date() {
        let cfg = cfg();
        let mut e = Entry::file("old");
        // 1960-ish: `duration_since` returns Err and the seconds are negative.
        e.mtime = Some(UNIX_EPOCH - Duration::from_secs(315_619_200));
        let text = date_text(&e, &cfg);
        assert!(text.starts_with("1960-"), "{text:?}");
    }

    #[test]
    fn an_mtime_row_with_a_bogus_date_still_renders_a_full_width_row() {
        let cfg = cfg();
        let mut e = Entry::file("farfuture");
        e.mtime = Some(UNIX_EPOCH + Duration::from_secs(9_999_999_999_999));
        let alloc = allocate(&cfg, 100);
        let line = row_text(&e, &alloc, &cfg, false, true);
        assert_eq!(text::width(&line), alloc.total_width());
    }

    #[test]
    fn an_entry_with_no_mtime_has_an_empty_date_cell() {
        assert_eq!(date_text(&Entry::file("a"), &cfg()), "");
    }

    #[test]
    fn a_rendered_row_is_exactly_the_inner_width_wide() {
        let cfg = cfg();
        let mut e = Entry::file("日本語のとても長いファイル名.txt");
        e.size = 362_333;
        e.mode = 0o100_644;
        e.mtime = Some(UNIX_EPOCH + Duration::from_secs(1_760_000_000));
        for inner in 0..=200usize {
            let alloc = allocate(&cfg, inner);
            let line = row_text(&e, &alloc, &cfg, false, true);
            assert_eq!(
                text::width(&line),
                alloc.total_width(),
                "inner {inner}: {line:?}"
            );
            assert!(text::width(&line) <= inner, "inner {inner}");
        }
    }

    #[test]
    fn the_header_carries_the_arrow_on_the_sorted_column_only() {
        let cfg = cfg();
        let alloc = allocate(&cfg, 120);
        let sort = SortState {
            key: crate::panel::SortKey::Column(ColumnId::Ext),
            reverse: false,
            secondary: None,
        };
        let line = header_text(&alloc, sort, false);
        assert!(line.contains("\u{25B2}Ext"), "{line}");
        assert!(line.contains("Name"), "{line}");
        assert!(!line.contains("\u{25B2}Name"), "{line}");

        let ascii = header_text(&alloc, sort, true);
        assert!(ascii.contains("^Ext"), "{ascii}");
        assert!(!ascii.contains('\u{25B2}'), "{ascii}");
    }

    fn sample_tab() -> Tab {
        let mut tab = Tab::new(crate::vfs::VfsPath::local("/x"));
        let mut a = Entry::file("a");
        a.size = 1024;
        let mut b = Entry::file("b");
        b.size = 2048;
        tab.entries = vec![Entry::parent_entry(), Entry::dir("d"), a, b];
        tab
    }

    #[test]
    fn with_nothing_marked_the_line_reports_the_directory() {
        // the first form. `..` is not counted.
        let tab = sample_tab();
        assert_eq!(
            status_text(&tab, &cfg(), false, &empty()),
            "3 k in 2 files, 1 dir"
        );
    }

    #[test]
    fn with_something_marked_the_line_reports_the_selection() {
        // the second form.
        let mut tab = sample_tab();
        tab.marks.insert("a".to_string());
        assert_eq!(
            status_text(&tab, &cfg(), false, &empty()),
            "1 of 3 selected \u{b7} 1 k"
        );
        assert_eq!(
            status_text(&tab, &cfg(), true, &empty()),
            "1 of 3 selected - 1 k"
        );
    }

    #[test]
    fn a_marked_directory_makes_the_size_a_lower_bound() {
        // a directory contributes to the counts always and to
        // the size only once sized. Nothing here has been sized, so the marker
        // is there and the figure is honest about being partial.
        let mut tab = sample_tab();
        tab.marks.insert("a".to_string());
        tab.marks.insert("d".to_string());
        assert_eq!(
            status_text(&tab, &cfg(), false, &empty()),
            "2 of 3 selected \u{b7} \u{2265} 1 k"
        );
        assert_eq!(
            status_text(&tab, &cfg(), true, &empty()),
            "2 of 3 selected - >= 1 k"
        );
    }

    #[test]
    fn a_selection_of_only_files_is_exact() {
        let mut tab = sample_tab();
        tab.marks.insert("a".to_string());
        tab.marks.insert("b".to_string());
        let got = status_text(&tab, &cfg(), false, &empty());
        assert!(!got.contains('\u{2265}'), "no bound is needed: {got}");
        assert_eq!(got, "2 of 3 selected \u{b7} 3 k");
    }

    #[test]
    fn singular_and_plural_both_read_correctly() {
        let mut tab = Tab::new(crate::vfs::VfsPath::local("/x"));
        tab.entries = vec![Entry::parent_entry(), Entry::file("only"), Entry::dir("d")];
        assert_eq!(
            status_text(&tab, &cfg(), false, &empty()),
            "0 k in 1 file, 1 dir"
        );
        tab.entries.push(Entry::dir("e"));
        tab.entries.push(Entry::file("another"));
        assert_eq!(
            status_text(&tab, &cfg(), false, &empty()),
            "0 k in 2 files, 2 dirs"
        );
    }

    #[test]
    fn sizes_follow_the_panel_size_settings() {
        let mut tab = sample_tab();
        tab.marks.insert("b".to_string());
        let mut c = cfg();
        c.human_sizes = true;
        assert!(
            status_text(&tab, &c, false, &empty()).ends_with("2.0 K"),
            "{}",
            status_text(&tab, &c, false, &empty())
        );
        let mut c = cfg();
        c.thousands_separator = false;
        let mut big = Entry::file("big");
        big.size = 12_345_678;
        tab.entries.push(big);
        tab.marks.clear();
        assert_eq!(
            status_text(&tab, &c, false, &empty()),
            "12060 k in 3 files, 1 dir",
            "no separators when the setting is off"
        );
    }
}
