//! File-type colouring.
//!
//! Rule-based on **extension and mode bits**, and configured in `config.toml`
//! rather than in the theme, so a user keeps their rules across a theme change.
//! A rule names a semantic slot; the theme decides what colour that slot is and
//! the renderer quantizes it for the session's colour depth.
//!
//! Resolution order, highest first:
//!
//! 1. `panel.marked_fg` - a marked entry, which wins over everything.
//!
//! 2. the first matching `[[filetypes]]` rule from `config.toml`, which may
//!    constrain the extension, the mode bits, or both. The shipped default
//!    carries the worked example - the archive extensions →
//!    `panel.archive_fg` - as an ordinary rule rather than as a hard-coded
//!    list, so a user can replace it.
//! 3. `panel.dir_fg` - a directory, or a symlink to one. Structural: a
//!    directory named `foo.zip` is a directory, which is why an `ext` rule
//!    never applies to one.
//! 4. `panel.link_fg` - a symlink.
//! 5. `panel.exec_fg` - the **mode bits**, never the extension
//!    (the design slot list, `Entry::is_executable`).
//! 6. `panel.fg`.

use crate::config::{Config, Rgb, Theme};
use crate::vfs::Entry;

/// The colour for one entry's row.
///
/// `marked` is whether the entry is in the tab's mark set; the design makes
/// it win over the file-type colour.
pub fn entry_fg(entry: &Entry, marked: bool, config: &Config, theme: &Theme) -> Rgb {
    if marked {
        return theme.panel.marked_fg;
    }
    // The user's own `[[filetypes]]` rules come first - the same precedence
    // the design gives user associations over the desktop's - with one
    // structural exception baked into `matches`: an `ext` rule never applies to
    // a directory, so a directory named `foo.zip` stays a directory.
    if let Some(rgb) = rule_fg(entry, config, theme) {
        return rgb;
    }
    if entry.is_parent || entry.is_dir() {
        return theme.panel.dir_fg;
    }
    if entry.is_symlink() {
        return theme.panel.link_fg;
    }
    if entry.is_executable() {
        return theme.panel.exec_fg;
    }
    theme.panel.fg
}

/// Does one rule match this entry?
///
/// A rule may constrain the extension, the mode bits, or both; both stated
/// means both must hold. A rule that constrains neither matches nothing, so an
/// empty `match = {}` is inert rather than colouring the whole panel.
fn matches(matcher: &crate::config::Matcher, entry: &Entry) -> bool {
    use crate::config::ModeMatch;

    let mut said_something = false;

    if let Some(mode) = matcher.mode {
        said_something = true;
        let ok = match mode {
            ModeMatch::Exec => entry.is_executable(),
            ModeMatch::Dir => entry.is_dir(),
            ModeMatch::Symlink => entry.is_symlink(),
        };
        if !ok {
            return false;
        }
    }

    if !matcher.ext.is_empty() {
        said_something = true;
        // An extension is a property of a *file* name. A directory called
        // `foo.zip` is a directory, and colouring it as an archive would hide
        // that (the design brackets directories for the same reason).
        if entry.is_dir() || entry.is_parent {
            return false;
        }
        let ext = entry.extension();
        if ext.is_empty()
            || !matcher
                .ext
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(ext))
        {
            return false;
        }
    }

    said_something
}

/// The first `[[filetypes]]` rule that matches, resolved through the theme.
///
/// A rule naming a slot the theme does not have is ignored rather than being an
/// error (a missing slot is never fatal).
fn rule_fg(entry: &Entry, config: &Config, theme: &Theme) -> Option<Rgb> {
    config
        .filetypes
        .iter()
        .filter(|rule| matches(&rule.matcher, entry))
        .find_map(|rule| theme.slot(&rule.slot))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, FiletypeRule, Matcher};
    use crate::vfs::{Entry, EntryKind};

    fn theme() -> Theme {
        Theme::blue()
    }

    #[test]
    fn marked_wins_over_everything() {
        let t = theme();
        let c = Config::default();
        let d = Entry::dir("bin");
        assert_eq!(entry_fg(&d, false, &c, &t), t.panel.dir_fg);
        assert_eq!(entry_fg(&d, true, &c, &t), t.panel.marked_fg);
    }

    #[test]
    fn executables_come_from_the_mode_bits_not_the_extension() {
        let t = theme();
        let c = Config::default();
        let mut plain = Entry::file("script.sh");
        plain.mode = 0o644;
        assert_eq!(entry_fg(&plain, false, &c, &t), t.panel.fg);

        let mut exec = Entry::file("run");
        exec.mode = 0o755;
        assert_eq!(entry_fg(&exec, false, &c, &t), t.panel.exec_fg);
    }

    #[test]
    fn symlinks_and_archives_have_their_own_slots() {
        let t = theme();
        let c = Config::default();
        let mut link = Entry::file("alias");
        link.kind = EntryKind::Symlink { to_dir: false };
        assert_eq!(entry_fg(&link, false, &c, &t), t.panel.link_fg);

        let arc = Entry::file("backup.TAR.GZ");
        assert_eq!(entry_fg(&arc, false, &c, &t), t.panel.archive_fg);
    }

    #[test]
    fn an_earlier_rule_beats_a_later_one() {
        let t = theme();
        let mut c = Config::default();
        c.filetypes.insert(
            0,
            FiletypeRule {
                matcher: Matcher {
                    ext: vec!["zip".to_string()],
                    mime: None,
                    mode: None,
                },
                slot: "syn.string".to_string(),
            },
        );
        let z = Entry::file("a.zip");
        assert_eq!(entry_fg(&z, false, &c, &t), t.syn.string);
    }

    #[test]
    fn a_rule_can_match_the_mode_bits() {
        use crate::config::ModeMatch;
        let t = theme();
        let mut c = Config::default();
        c.filetypes.insert(
            0,
            FiletypeRule {
                matcher: Matcher {
                    ext: Vec::new(),
                    mime: None,
                    mode: Some(ModeMatch::Exec),
                },
                slot: "syn.keyword".to_string(),
            },
        );
        let mut exe = Entry::file("run");
        exe.mode = 0o755;
        assert_eq!(entry_fg(&exe, false, &c, &t), t.syn.keyword);
        // A rule stating both an extension and a mode needs both to hold.
        c.filetypes.get_mut(0).expect("the rule").matcher.ext = vec!["sh".to_string()];
        assert_eq!(entry_fg(&exe, false, &c, &t), t.panel.exec_fg);
        let mut sh = Entry::file("run.sh");
        sh.mode = 0o755;
        assert_eq!(entry_fg(&sh, false, &c, &t), t.syn.keyword);
    }

    #[test]
    fn a_directory_named_like_an_archive_is_still_a_directory() {
        let t = theme();
        let c = Config::default();
        let d = Entry::dir("backup.zip");
        assert_eq!(entry_fg(&d, false, &c, &t), t.panel.dir_fg);
    }

    #[test]
    fn an_empty_matcher_colours_nothing() {
        let t = theme();
        let mut c = Config::default();
        c.filetypes.insert(
            0,
            FiletypeRule {
                matcher: Matcher {
                    ext: Vec::new(),
                    mime: None,
                    mode: None,
                },
                slot: "syn.string".to_string(),
            },
        );
        let f = Entry::file("a.txt");
        assert_eq!(entry_fg(&f, false, &c, &t), t.panel.fg);
    }

    #[test]
    fn the_shipped_default_carries_the_spec_19_archive_rule() {
        let t = theme();
        let c = Config::default();
        c.filetypes
            .iter()
            .find(|r| r.slot == "panel.archive_fg")
            .expect("the archive rule ships as a default");
        let z = Entry::file("a.zip");
        assert_eq!(entry_fg(&z, false, &c, &t), t.panel.archive_fg);
    }

    #[test]
    fn the_first_matching_rule_wins_so_order_is_the_users_lever() {
        let t = theme();
        let mut c = Config::default();
        // Appended *after* the shipped archive rule, so the archive rule wins.
        c.filetypes.push(FiletypeRule {
            matcher: Matcher {
                ext: vec!["zip".to_string()],
                mime: None,
                mode: None,
            },
            slot: "syn.string".to_string(),
        });
        let z = Entry::file("a.zip");
        assert_eq!(entry_fg(&z, false, &c, &t), t.panel.archive_fg);
    }

    #[test]
    fn a_rule_naming_an_unknown_slot_is_ignored() {
        let t = theme();
        let mut c = Config::default();
        c.filetypes.push(FiletypeRule {
            matcher: Matcher {
                ext: vec!["txt".to_string()],
                mime: None,
                mode: None,
            },
            slot: "nope.nope".to_string(),
        });
        let f = Entry::file("a.txt");
        assert_eq!(entry_fg(&f, false, &c, &t), t.panel.fg);
    }
}
