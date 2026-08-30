//! The `Ctrl+C` / `Ctrl+X` / `Ctrl+V` clipboard.
//!
//! A second way to copy, alongside `F5`, answering a different question. `F5`
//! is "put these there, now", and wants both panels already pointing at the
//! right places. This is "remember this" - you then navigate, possibly for a
//! while, and put it down. The target is chosen *after* the source, which is
//! the point and is not something `F5` can express.
//!
//! It holds **paths, not bytes**, so nothing is read until the paste and a
//! large directory costs nothing to hold.

use crate::vfs::VfsPath;

/// What `Ctrl+C` or `Ctrl+X` remembered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clipboard {
    /// What to act on.
    pub paths: Vec<VfsPath>,
    /// `Ctrl+X` rather than `Ctrl+C`: the source is removed after a successful
    /// copy, and the clipboard is spent.
    pub cut: bool,
}

impl Clipboard {
    /// Remember one entry.
    pub fn one(path: VfsPath, cut: bool) -> Self {
        Self {
            paths: vec![path],
            cut,
        }
    }

    /// What the status line says while this is held, so `Ctrl+V` is never a
    /// guess about what is about to land.
    pub fn describe(&self) -> String {
        let verb = if self.cut { "cut" } else { "copied" };
        match self.paths.split_first() {
            None => String::new(),
            Some((only, [])) => format!("{verb}: {}", only.file_name().unwrap_or_default()),
            Some((first, rest)) => format!(
                "{verb}: {} and {} more",
                first.file_name().unwrap_or_default(),
                rest.len()
            ),
        }
    }
}

/// The name a paste into the source's own directory takes.
///
/// Copying something into the directory it is already in cannot keep its name,
/// and it is the common case - it is how a file gets a backup before being
/// edited - so the design makes it a rename rather than a conflict:
///
/// ```text
/// report.json        →  report Copy 1.json
/// report Copy 1.json →  report Copy 2.json
/// photos             →  photos Copy 1
/// ```
///
/// `taken` decides whether a candidate is in the way; the caller supplies it so
/// this stays a pure function and the tests need no filesystem.
pub fn copy_name(name: &str, is_dir: bool, mut taken: impl FnMut(&str) -> bool) -> String {
    let (stem, ext) = split_name(name, is_dir);
    // A name that already ends in ` Copy <n>` is renumbered, not stacked, so
    // repeated pastes give `Copy 1`, `Copy 2`, `Copy 3` rather than
    // `Copy 1 Copy 1`.
    let base = strip_copy_suffix(stem);
    for n in 1..=u32::MAX {
        let candidate = join_name(&format!("{base} Copy {n}"), ext);
        if !taken(&candidate) {
            return candidate;
        }
    }
    // Unreachable in practice; a name rather than a panic if it ever is not.
    join_name(&format!("{base} Copy"), ext)
}

/// Split off the extension the way the `ext` column does, so the
/// program has one definition of the word. A directory has none, and a leading
/// dot is not a separator - `.bashrc` is a whole name, or the copy would come
/// out as ` Copy 1.bashrc`.
fn split_name(name: &str, is_dir: bool) -> (&str, &str) {
    if is_dir {
        return (name, "");
    }
    match name.rfind('.') {
        Some(0) | None => (name, ""),
        Some(idx) => {
            let (stem, rest) = name.split_at(idx);
            (stem, rest.get(1..).unwrap_or(""))
        }
    }
}

fn join_name(stem: &str, ext: &str) -> String {
    if ext.is_empty() {
        stem.to_string()
    } else {
        format!("{stem}.{ext}")
    }
}

/// `report Copy 3` → `report`. Leaves anything else alone.
fn strip_copy_suffix(stem: &str) -> &str {
    let Some(at) = stem.rfind(" Copy ") else {
        return stem;
    };
    let tail = stem.get(at + " Copy ".len()..).unwrap_or("");
    if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) {
        stem.get(..at).unwrap_or(stem)
    } else {
        stem
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn free(_: &str) -> bool {
        false
    }

    #[test]
    fn a_copy_keeps_its_extension() {
        assert_eq!(copy_name("report.json", false, free), "report Copy 1.json");
    }

    #[test]
    fn a_directory_has_no_extension_to_keep() {
        assert_eq!(copy_name("photos", true, free), "photos Copy 1");
        // ...even one that looks like it does.
        assert_eq!(copy_name("photos.bak", true, free), "photos.bak Copy 1");
    }

    #[test]
    fn copies_are_renumbered_rather_than_stacked() {
        assert_eq!(
            copy_name("report Copy 1.json", false, free),
            "report Copy 1.json",
        );
        // Explicitly: the suffix is stripped before the new one is added.
        assert_eq!(
            copy_name("report Copy 7.json", false, free),
            "report Copy 1.json"
        );
        assert_eq!(copy_name("report Copy 2", true, free), "report Copy 1");
    }

    #[test]
    fn the_number_rises_until_the_name_is_free() {
        let existing = ["report Copy 1.json", "report Copy 2.json"];
        let taken = |c: &str| existing.contains(&c);
        assert_eq!(copy_name("report.json", false, taken), "report Copy 3.json");
    }

    #[test]
    fn a_dotfile_keeps_its_leading_dot() {
        // ` Copy 1.bashrc` would be the alternative, which is worse than useless.
        assert_eq!(copy_name(".bashrc", false, free), ".bashrc Copy 1");
    }

    #[test]
    fn only_a_real_copy_suffix_is_stripped() {
        assert_eq!(
            copy_name("Copy of report.txt", false, free),
            "Copy of report Copy 1.txt"
        );
        assert_eq!(
            copy_name("report Copy x.txt", false, free),
            "report Copy x Copy 1.txt"
        );
    }

    #[test]
    fn the_double_extension_case_is_consistent_with_the_ext_column() {
        // one definition of "extension" in the program beats a
        // prettier answer here.
        assert_eq!(
            copy_name("archive.tar.gz", false, free),
            "archive.tar Copy 1.gz"
        );
    }

    #[test]
    fn the_status_line_names_what_is_held() {
        let one = Clipboard::one(VfsPath::local("/a/b.txt"), false);
        assert_eq!(one.describe(), "copied: b.txt");
        let cut = Clipboard::one(VfsPath::local("/a/b.txt"), true);
        assert_eq!(cut.describe(), "cut: b.txt");
    }
}
