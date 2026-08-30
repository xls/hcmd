//! What one file is, from the two things that know: the filesystem and the
//! bytes.
//!
//! The dialog behind this opens from two places, the viewer, which already has
//! the bytes, and the panel, which already has the entry under the cursor, and
//! both ask the same question. So this takes what a caller already has and
//! interprets it, and does no I/O of its own: a draw path that stats a file is
//! a draw path that blocks on a dead network mount.
//!
//! The answer always has a first half. A name, a size and an attribute string
//! are facts about every file, including the ones no template recognises, so
//! there is no empty case: a file nothing claims still opens with its name,
//! its size, its attributes and a line saying the contents were not
//! recognised.

use std::sync::OnceLock;

use super::summary::{SummaryLine, best_match, heading, summary};
use super::template::Template;
use super::template_data::BUILTIN;
use crate::panel::format::{human_size, thousands};

/// How many bytes of a file to read before asking what it is.
///
/// Not "the first few kilobytes": some structures are a long way in. The UDF
/// anchor descriptor is at 524288, the btrfs superblock at 65536 and the ISO
/// 9660 volume descriptor at 32768, and a template whose bytes were never read
/// cannot match. The furthest byte any built-in template needs is 524320, the
/// end of the UDF anchor descriptor; this is that rounded up to 640 KiB, and
/// `builtin_templates_all_fit_in_the_head` in the tests is what keeps it true
/// as templates are added.
///
/// A caller reads `min(file length, HEAD_BYTES)`; a short read is not a
/// problem, it just means the templates that live past the end of the file do
/// not match, which is the right answer for a file that short.
pub const HEAD_BYTES: usize = 640 * 1024;

/// How much of a file an *automatic* recognition may read.
///
/// Deliberately far smaller than [`HEAD_BYTES`], which is what the file
/// information dialog reads when a person asked a question about one file.
/// This one runs on every file that is opened and on every entry into hex
/// mode, so it is a fixed, small, constant read rather than a fraction of the
/// file.
///
/// 4 kB reaches every signature a file carries near its front, the boot
/// sector's at 510 and the ext superblock's at 1080 included. It deliberately
/// does not reach the ISO 9660 descriptor at 32768 or the btrfs superblock at
/// 65536: those live in megabyte-scale disk images, where reading 64 kB to
/// guess at a format nobody asked about is the wrong trade. `t` in hex mode
/// applies them in one keystroke, and the file information dialog reads the
/// full head because it was asked to.
pub const MATCH_HEAD: usize = 4096;

/// Everything a summary can say about one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    /// The file's name, for the heading.
    pub name: String,
    /// Name, size, attributes and date: true of every file, template or not.
    pub facts: Vec<SummaryLine>,
    /// What the contents turned out to be, where anything recognised them.
    pub format: Option<String>,
    /// The format's own summary lines, empty where there is no format or the
    /// template that matched carries no summary.
    pub lines: Vec<SummaryLine>,
    /// Why there is no format, in words, where there is none.
    pub note: Option<String>,
}

/// What the caller already knows about the file, from the filesystem.
///
/// Borrowed rather than owned because every caller has these in hand: the
/// panel has the entry it is drawing and the viewer has the path it opened.
#[derive(Debug, Clone, Copy)]
pub struct FileFacts<'a> {
    /// The file name, as the panel shows it.
    pub name: &'a str,
    /// Its length in bytes.
    pub size: u64,
    /// The attribute string, already rendered by
    /// [`crate::panel::format::attr_text`] so the two agree.
    pub attrs: &'a str,
    /// The modification date, already rendered by
    /// [`crate::panel::format::date_text`]. `None` leaves the line out.
    pub modified: Option<&'a str>,
    /// True for a directory, which has contents but no bytes to recognise.
    pub is_dir: bool,
}

impl<'a> FileFacts<'a> {
    /// The three facts every caller has.
    #[must_use]
    pub const fn new(name: &'a str, size: u64, attrs: &'a str) -> Self {
        Self {
            name,
            size,
            attrs,
            modified: None,
            is_dir: false,
        }
    }

    /// With the date the panel is already showing.
    #[must_use]
    pub const fn modified(mut self, date: &'a str) -> Self {
        self.modified = Some(date);
        self
    }

    /// Marked as a directory.
    #[must_use]
    pub const fn directory(mut self, yes: bool) -> Self {
        self.is_dir = yes;
        self
    }
}

/// Every built-in template, parsed once.
///
/// Parsed on first use rather than at startup: a program that never opens this
/// dialog should not pay for it, and a hundred small TOML files is a few
/// milliseconds once.
#[must_use]
pub fn builtin() -> &'static [Template] {
    static PARSED: OnceLock<Vec<Template>> = OnceLock::new();
    PARSED
        .get_or_init(|| {
            let mut found: Vec<Template> = BUILTIN
                .iter()
                .filter_map(|text| Template::parse(text).ok())
                .collect();
            found.sort_by_key(|t| t.name.to_lowercase());
            found
        })
        .as_slice()
}

/// A size, both ways: exactly, and the way the panel writes it.
fn size_line(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{} bytes", thousands(bytes));
    }
    format!("{} bytes ({}B)", thousands(bytes), human_size(bytes))
}

/// Describe one file: its filesystem facts, and its contents where a template
/// recognises them.
///
/// `head` is the front of the file, at most [`HEAD_BYTES`] of it. An empty
/// `head` is not an error and neither is a short one; both simply mean fewer
/// templates can claim the file.
#[must_use]
pub fn describe(facts: &FileFacts<'_>, head: &[u8]) -> FileInfo {
    describe_with(builtin(), facts, head)
}

/// [`describe`], against a template set the caller chose.
///
/// The one the program uses is [`builtin`]; this exists so a test can put a
/// known set in front of it, and so templates fetched at runtime can be added
/// to the built-in ones without this module knowing where they came from.
#[must_use]
pub fn describe_with(templates: &[Template], facts: &FileFacts<'_>, head: &[u8]) -> FileInfo {
    let mut lines = vec![SummaryLine {
        label: "Size".to_string(),
        value: size_line(facts.size),
    }];
    if !facts.attrs.is_empty() {
        lines.push(SummaryLine {
            label: "Attributes".to_string(),
            value: facts.attrs.to_string(),
        });
    }
    if let Some(date) = facts.modified {
        lines.push(SummaryLine {
            label: "Modified".to_string(),
            value: date.to_string(),
        });
    }

    let mut info = FileInfo {
        name: facts.name.to_string(),
        facts: lines,
        format: None,
        lines: Vec::new(),
        note: None,
    };

    // A directory is not unrecognised, it is a different kind of thing, and
    // saying "the contents were not recognised" about one would be answering a
    // question nobody asked.
    if facts.is_dir {
        info.note = Some("A directory. Enter opens it.".to_string());
        return info;
    }
    if facts.size == 0 {
        info.note = Some("The file is empty.".to_string());
        return info;
    }
    let Some(template) = best_match(templates, head) else {
        info.note = Some("The contents match no template this program has.".to_string());
        return info;
    };
    info.format = Some(heading(template).to_string());
    info.lines = summary(template, head);
    if info.lines.is_empty() {
        info.note = Some(format!(
            "Recognised as {}, which has no summary of its own. The hex viewer can apply it as a template.",
            template.name
        ));
    }
    info
}

#[cfg(test)]
#[path = "fileinfo_tests.rs"]
mod tests;
