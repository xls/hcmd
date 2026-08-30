//! Reading a server's directory listing.
//!
//! Two formats, and they are not equally trustworthy. `MLSD` is machine
//! readable and says what it means, so it is used wherever the server has it.
//! `LIST` is a human-facing table whose shape depends on the server, and every
//! field in it is a guess: this parses the common Unix and DOS shapes and
//! treats anything it cannot read as a row it did not understand rather than
//! as a file with wrong facts.
//!
//! A year is the part `LIST` most often will not give. A listing that shows a
//! time instead of a year means "this year" by convention, and a date that
//! would then be in the future means last year, which is the only reading that
//! is ever right and is still a guess. That is why a size or a date from
//! `LIST` is worth less than one from `MLSD`, and why the two are parsed
//! separately rather than into one lenient reader.

use super::*;

/// Turn an `MLSD` response into rows.
///
/// `type=cdir` and `type=pdir` are dropped: they are the directory itself and
/// its parent, and `RemoteFs::read_dir` adds the `..` row the way every other
/// backend does.
///
/// A server that answered `FEAT` with `MLSD` and then sent something that is
/// not MLSD is [`Error::Unsupported`], which is what [`FtpFs::list`] reads as
/// "ask this server for a `LIST` instead, and stop believing its `FEAT`".
pub(super) fn entries_from_mlsd(lines: &[String]) -> Result<Vec<Entry>> {
    let entries: Vec<Entry> = lines
        .iter()
        .filter_map(|line| parse_mlsd(line.as_str()))
        .collect();
    if entries.is_empty() && lines.iter().any(|line| !line.trim().is_empty()) {
        return Err(Error::Unsupported("MLSD"));
    }
    Ok(entries)
}

/// Turn a `LIST` response into rows, or say that the dialect was not one of
/// the ones this backend knows.
///
/// A line that does not parse is skipped, because a listing with one odd line
/// in it is still a listing. A directory in which *nothing* parsed is an
/// error: an empty panel would be a claim that the directory is empty, and
/// this backend does not know that (see the module documentation).
pub(super) fn entries_from_list(dir: &str, lines: &[String]) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let mut candidates = 0usize;
    for line in lines {
        if !is_listing_row(line) {
            continue;
        }
        candidates += 1;
        if let Some(entry) = parse_list(line) {
            entries.push(entry);
        }
    }
    if entries.is_empty() && candidates > 0 {
        return Err(Error::msg(format!(
            "{dir}: the server's LIST format was not recognised \
             (Unix and DOS listings are understood; MLSD is preferred and this server has none)"
        )));
    }
    Ok(entries)
}

/// Whether a `LIST` line is claiming to be a row at all.
///
/// A Unix server prefixes its listing with `total 8`, and blank lines happen;
/// neither is a row and neither is evidence about the dialect.
fn is_listing_row(line: &str) -> bool {
    let line = line.trim();
    if line.is_empty() {
        return false;
    }
    if let Some(rest) = line.strip_prefix("total ") {
        return !rest.trim().chars().all(|c| c.is_ascii_digit());
    }
    true
}

/// Parse one `MLSD` line into an [`Entry`] (RFC 3659).
///
/// The shape is `fact=value;fact=value; name`, the facts never contain a
/// space, and the name is everything after the first one - which is what
/// makes a name with spaces in it parse correctly.
///
/// `None` for a line that is not that shape, and for the `cdir` and `pdir`
/// rows, which name the directory itself and its parent.
///
/// **No mode, no owner, no link**: `perm` is a capability string and
/// `UNIX.mode` is the server's opinion about a permission model this backend
/// does not present.
pub fn parse_mlsd(line: &str) -> Option<Entry> {
    let line = line.trim_start();
    let space = line.find(' ')?;
    let facts = line.get(..space)?;
    let name = line.get(space + 1..)?.trim_end_matches(['\r', '\n']);
    if name.is_empty() {
        return None;
    }
    let mut kind = EntryKind::File;
    let mut size = 0u64;
    let mut mtime = None;
    let mut seen_type = false;
    for fact in facts.split(';') {
        let Some((key, value)) = fact.split_once('=') else {
            continue;
        };
        if key.eq_ignore_ascii_case("type") {
            seen_type = true;
            kind = match value.to_ascii_lowercase().as_str() {
                "dir" => EntryKind::Dir,
                "file" => EntryKind::File,
                // The directory itself and its parent are not rows.
                "cdir" | "pdir" => return None,
                // `type=OS.unix=slink:/target`. FTP has no readlink, so this
                // is reported as neither a file nor a directory rather than
                // as a link this backend cannot follow.
                _ => EntryKind::Other,
            };
        } else if key.eq_ignore_ascii_case("size") {
            size = value.parse().unwrap_or(0);
        } else if key.eq_ignore_ascii_case("modify") {
            mtime = parse_mlsd_time(value);
        }
    }
    // A line with no `fact=value` at all is not an MLSD line; saying so
    // here is what keeps a LIST line from being read as a nameless file.
    if !facts.contains('=') {
        return None;
    }
    let _ = seen_type;
    let base = basename(name);
    // The same guard the two `LIST` dialects carry: a row is a row only if its
    // name is a name (`crate::vfs::is_plain_name`).
    if !crate::vfs::is_plain_name(base) {
        return None;
    }
    let mut entry = match kind {
        EntryKind::Dir => Entry::dir(base),
        EntryKind::File => Entry::file(base),
        EntryKind::Symlink { .. } | EntryKind::Other => Entry {
            kind: EntryKind::Other,
            ..Entry::file(base)
        },
    };
    entry.size = if matches!(entry.kind, EntryKind::Dir) {
        0
    } else {
        size
    };
    entry.mtime = mtime;
    Some(entry)
}

/// `MLST`'s answer is one `MLSD` line whose name is the path that was asked
/// about, so the row it produces is the same row with a file name on it.
pub(super) fn parse_mlst(line: &str) -> Option<Entry> {
    parse_mlsd(line)
}

/// RFC 3659's `YYYYMMDDHHMMSS[.sss]`, in UTC.
fn parse_mlsd_time(value: &str) -> Option<SystemTime> {
    let digits = value.split('.').next().unwrap_or(value);
    if digits.len() < 14 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let year: i32 = digits.get(0..4)?.parse().ok()?;
    let month: u32 = digits.get(4..6)?.parse().ok()?;
    let day: u32 = digits.get(6..8)?.parse().ok()?;
    let hour: u32 = digits.get(8..10)?.parse().ok()?;
    let minute: u32 = digits.get(10..12)?.parse().ok()?;
    let second: u32 = digits.get(12..14)?.parse().ok()?;
    utc(year, month, day, hour, minute, second)
}

/// Parse one `LIST` line into an [`Entry`], for the dialects the module
/// documentation names. `None` for anything else, and for the `.` and `..`
/// rows a Unix listing includes.
///
/// **The time is a guess and the size is not.** `LIST` has no specification,
/// and a Unix listing's time carries no zone: the server prints its own local
/// time and never says which that is. It is read here as UTC, so a row can be
/// out by the server's offset. `MLSD` is preferred everywhere for exactly this
/// reason, and `Capabilities::FTP` is what tells the panel not to promise more.
pub fn parse_list(line: &str) -> Option<Entry> {
    parse_unix_list(line).or_else(|| parse_dos_list(line))
}

/// One token of a listing line, with the byte offset it started at, so that a
/// name containing spaces can be recovered from the original line.
fn tokenize(line: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (index, ch) in line.char_indices() {
        if ch.is_whitespace() {
            if let Some(from) = start.take()
                && let Some(token) = line.get(from..index)
            {
                out.push((from, token));
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(from) = start
        && let Some(token) = line.get(from..)
    {
        out.push((from, token));
    }
    out
}

/// The Unix `ls -l` dialect, in both its spellings:
///
/// ```text
/// drwxr-xr-x   2 ftp      ftp          4096 Nov  5 13:46 pub
/// -rw-r--r--   1 1000     1000      1048576 Jan 10  2024 disk image.iso
/// lrwxrwxrwx   1 root     root            9 Feb 14 09:03 latest -> 1.2.3
/// -rw-r--r--   1 ftp      ftp          1024 2024-01-10 13:46 iso-dated.txt
/// ```
///
/// The mode string is read only to tell a directory from a file and a link
/// from both; its bits are **not** carried onto the row.
fn parse_unix_list(line: &str) -> Option<Entry> {
    let tokens = tokenize(line);
    let (_, mode) = tokens.first()?;
    let kind = unix_kind(mode)?;
    // The size is the token before the date, and the date is what is found by
    // shape rather than by position: servers disagree about how many columns
    // sit between the mode and the size.
    for at in 2..tokens.len() {
        let (_, token) = tokens.get(at)?;
        let (mtime, name_at) = if let Some(month) = month_number(token) {
            let (_, day) = tokens.get(at + 1)?;
            let (_, when) = tokens.get(at + 2)?;
            let (name_start, _) = tokens.get(at + 3)?;
            (unix_time_of(month, day, when)?, *name_start)
        } else if let Some((year, month, day)) = iso_date(token) {
            let (_, when) = tokens.get(at + 1)?;
            let (name_start, _) = tokens.get(at + 2)?;
            let (hour, minute, second) = clock(when)?;
            (utc(year, month, day, hour, minute, second), *name_start)
        } else {
            continue;
        };
        let (_, size) = tokens.get(at.checked_sub(1)?)?;
        let size: u64 = size.parse().ok()?;
        let raw = line.get(name_at..)?.trim_end();
        let raw = match kind {
            // `name -> target`: the target is not a row and this backend has
            // no way to ask about it again.
            EntryKind::Other => raw.split(" -> ").next().unwrap_or(raw),
            EntryKind::Dir | EntryKind::File | EntryKind::Symlink { .. } => raw,
        };
        // The last component only, exactly as [`parse_mlsd`] does it and for
        // the same two reasons: a server answering `LIST /pub` may write the
        // path it was given, and a name carrying `../` is joined onto a local
        // destination by `ops::copy` as written, which is Zip Slip's remote
        // spelling (`crate::vfs::is_plain_name`). MLSD and LIST
        // must not disagree about this: whether a server advertises MLSD is
        // not the user's choice.
        let name = basename(raw);
        if !crate::vfs::is_plain_name(name) {
            return None;
        }
        let mut entry = match kind {
            EntryKind::Dir => Entry::dir(name),
            EntryKind::File => Entry::file(name),
            EntryKind::Symlink { .. } | EntryKind::Other => Entry {
                kind: EntryKind::Other,
                ..Entry::file(name)
            },
        };
        entry.size = if matches!(kind, EntryKind::Dir) {
            0
        } else {
            size
        };
        entry.mtime = mtime;
        return Some(entry);
    }
    None
}

/// The type character and the nine mode characters of an `ls -l` line, with
/// the ACL marker some servers append. `None` when the token is not one, which
/// is how a line of another dialect is rejected in one comparison.
fn unix_kind(token: &str) -> Option<EntryKind> {
    let mut chars = token.chars();
    let first = chars.next()?;
    let rest: String = chars.collect();
    let bits = rest.trim_end_matches(['+', '.', '@']);
    if bits.len() != 9 || !bits.chars().all(|c| "rwxsStTlL-".contains(c)) {
        return None;
    }
    match first {
        'd' => Some(EntryKind::Dir),
        '-' | 'f' => Some(EntryKind::File),
        // A link, a socket, a fifo, a device: none of them is a file this
        // backend can promise anything about.
        'l' | 's' | 'p' | 'b' | 'c' | 'D' => Some(EntryKind::Other),
        _ => None,
    }
}

/// `Jan` to `Dec`, in the C locale, which is what every server that means to
/// be parsed prints.
fn month_number(token: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let lower = token.to_ascii_lowercase();
    MONTHS
        .iter()
        .position(|month| *month == lower)
        .and_then(|index| u32::try_from(index + 1).ok())
}

/// `2024-01-10`.
fn iso_date(token: &str) -> Option<(i32, u32, u32)> {
    let mut parts = token.split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some((year, month, day))
}

/// `13:46` or `13:46:59`.
fn clock(token: &str) -> Option<(u32, u32, u32)> {
    let mut parts = token.split(':');
    let hour: u32 = parts.next()?.parse().ok()?;
    let minute: u32 = parts.next()?.parse().ok()?;
    let second: u32 = match parts.next() {
        Some(text) => text.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some((hour, minute, second))
}

/// A Unix listing's `Nov  5 13:46` or `Jan 10  2024`.
///
/// The first spelling has no year: `ls` prints the time for a recent file and
/// the year for an old one. The year is taken to be this one, and the previous
/// one when that would put the file in the future - the same rule every other
/// client uses, and the reason this value is documented as a guess.
fn unix_time_of(month: u32, day: &str, when: &str) -> Option<Option<SystemTime>> {
    let day: u32 = day.parse().ok()?;
    if when.len() == 4
        && let Ok(year) = when.parse::<i32>()
    {
        return Some(utc(year, month, day, 0, 0, 0));
    }
    let (hour, minute, second) = clock(when)?;
    let year = current_year();
    let candidate = utc(year, month, day, hour, minute, second);
    let Some(time) = candidate else {
        return Some(None);
    };
    let ahead = time
        .duration_since(SystemTime::now())
        .map(|gap| gap > Duration::from_secs(60 * 60 * 24))
        .unwrap_or(false);
    if ahead {
        Some(utc(year - 1, month, day, hour, minute, second))
    } else {
        Some(Some(time))
    }
}

/// The DOS dialect, as IIS prints it in MS-DOS listing mode:
///
/// ```text
/// 01-10-24  01:46PM       <DIR>          folder name
/// 01-10-24  01:46PM              1234 file.txt
/// 2024-01-10  13:46         1234 iso-ish.txt
/// ```
fn parse_dos_list(line: &str) -> Option<Entry> {
    let tokens = tokenize(line);
    let (_, date) = tokens.first()?;
    let (_, time) = tokens.get(1)?;
    let (year, month, day) = dos_date(date)?;
    let (hour, minute) = dos_time(time)?;
    let (_, third) = tokens.get(2)?;
    let (name_start, _) = tokens.get(3)?;
    // Basename'd for the reason [`parse_unix_list`] gives: a listing name is
    // a name and never a path.
    let name = basename(line.get(*name_start..)?.trim_end());
    if !crate::vfs::is_plain_name(name) {
        return None;
    }
    let mut entry = if third.eq_ignore_ascii_case("<DIR>") {
        Entry::dir(name)
    } else {
        let mut entry = Entry::file(name);
        entry.size = third.replace([',', '.', '\u{a0}'], "").parse().ok()?;
        entry
    };
    entry.mtime = utc(year, month, day, hour, minute, 0);
    Some(entry)
}

/// `01-10-24`, `01-10-2024` or `2024-01-10`, with `/` as well as `-`.
///
/// A two-digit year is read the way every FTP client reads it: `70` and above
/// is the nineteen-hundreds, below it is the two-thousands.
fn dos_date(token: &str) -> Option<(i32, u32, u32)> {
    let parts: Vec<&str> = token.split(['-', '/']).collect();
    let (first, second, third) = match parts.as_slice() {
        [a, b, c] => (*a, *b, *c),
        _ => return None,
    };
    if first.len() == 4 {
        let year: i32 = first.parse().ok()?;
        let month: u32 = second.parse().ok()?;
        let day: u32 = third.parse().ok()?;
        return in_range(year, month, day);
    }
    let month: u32 = first.parse().ok()?;
    let day: u32 = second.parse().ok()?;
    let year: i32 = third.parse().ok()?;
    let year = match third.len() {
        2 if year >= 70 => 1900 + year,
        2 => 2000 + year,
        4 => year,
        _ => return None,
    };
    in_range(year, month, day)
}

/// A date that could be a date.
fn in_range(year: i32, month: u32, day: u32) -> Option<(i32, u32, u32)> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || !(1900..=9999).contains(&year) {
        return None;
    }
    Some((year, month, day))
}

/// `01:46PM`, `01:46 PM` having already been split, or a plain `13:46`.
fn dos_time(token: &str) -> Option<(u32, u32)> {
    let upper = token.to_ascii_uppercase();
    let (body, shift) = if let Some(body) = upper.strip_suffix("AM") {
        (body, 0)
    } else if let Some(body) = upper.strip_suffix("PM") {
        (body, 12)
    } else {
        (upper.as_str(), -1)
    };
    let (hour, minute, _) = clock(body.trim())?;
    match shift {
        0 => Some((if hour == 12 { 0 } else { hour }, minute)),
        12 => Some((if hour == 12 { 12 } else { hour + 12 }, minute)),
        _ => Some((hour, minute)),
    }
}

/// The file name of a name that may be a path.
///
/// A listing name is a name: MLSD, LIST and the DOS dialect all reduce to the
/// last component here, so a server cannot choose which parser gets to hand
/// `ops::copy` a `../`.
fn basename(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// A calendar date in UTC as a [`SystemTime`], or `None` when it is not a date.
pub(super) fn utc(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Option<SystemTime> {
    let date = chrono::NaiveDate::from_ymd_opt(year, month, day)?;
    let stamp = date
        .and_hms_opt(hour, minute, second)?
        .and_utc()
        .timestamp();
    unix_time(stamp)
}

/// A Unix timestamp as a [`SystemTime`]. `None` before 1970, which no listing
/// this program will meet has and which has no representation here.
pub(super) fn unix_time(stamp: i64) -> Option<SystemTime> {
    let seconds = u64::try_from(stamp).ok()?;
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

/// This year, in UTC, for the Unix listing that omits it.
fn current_year() -> i32 {
    chrono::Utc::now().year()
}
