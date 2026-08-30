//! What a running quick search looks like in the panel status line.
//!
//!
//! the design pins the rendering: while a search is active the panel status line
//! shows the buffer **and its case mode**, as
//!
//! ```text
//! search: Tho [Aa]
//! ```
//!
//! so it is never a mystery why a file did not match. The buffer lives on the
//! panel ([`crate::panel::QuickSearch`]) and the case setting lives in
//! `config.panel.quick_search_case`; this module is the one place that turns
//! the two into that string, so the renderer and the input model cannot
//! disagree about it.
//!
//! # The case marker
//!
//! The marker names the **effective** case handling - what `smart` resolved to
//! for the buffer that has actually been typed, not the configured mode, since
//! the configured mode is not what a user needs to see mid-search:
//!
//! | Effective | Marker | When |
//! |---|---|---|
//! | case-sensitive | `[Aa]` | `sensitive`, or `smart` once an uppercase character is typed |
//! | case-insensitive | `[aa]` | `insensitive`, or `smart` while the buffer is all lowercase |
//!
//! the design shows only `search: Tho [Aa]`, and `Tho` under the default `smart`
//! mode is case-sensitive, which is what fixes `[Aa]` to that half of the
//! table.

use crate::config::{QuickSearchCase, QuickSearchMode};
use crate::panel::Panel;

/// The marker shown when matching distinguishes case.
pub const CASE_SENSITIVE_MARK: &str = "[Aa]";
/// The marker shown when matching folds case.
pub const CASE_INSENSITIVE_MARK: &str = "[aa]";

/// Does this buffer match case-sensitively under this setting?
///
/// `smart` is the ripgrep convention: insensitive while the buffer is all
/// lowercase, sensitive the moment an uppercase character is typed.
/// This is the same rule [`quick_match`] applies, stated once
/// so the status line cannot claim something the matcher does not do.
pub fn is_case_sensitive(buffer: &str, case: QuickSearchCase) -> bool {
    match case {
        QuickSearchCase::Sensitive => true,
        QuickSearchCase::Insensitive => false,
        QuickSearchCase::Smart => buffer.chars().any(char::is_uppercase),
    }
}

/// Does `name` match a quick-search `query`?
///
/// The one matcher, so that the panel, the drives popup, the open-with list,
/// the command history and the host list all answer the same question the same
/// way, and so that the `[Aa]` marker [`case_indicator`] draws is a statement
/// about this function rather than a second opinion.
///
/// An empty query matches everything: a search that has been armed and not yet
/// typed into has excluded nothing.
pub fn quick_match(name: &str, query: &str, mode: QuickSearchMode, case: QuickSearchCase) -> bool {
    if query.is_empty() {
        return true;
    }
    let (name, query) = if is_case_sensitive(query, case) {
        (name.to_string(), query.to_string())
    } else {
        (name.to_lowercase(), query.to_lowercase())
    };

    match mode {
        QuickSearchMode::Prefix => name.starts_with(&query),
        QuickSearchMode::Substring => name.contains(&query),
        QuickSearchMode::Fuzzy => {
            let mut haystack = name.chars();
            query.chars().all(|needle| haystack.any(|c| c == needle))
        }
    }
}

/// The `[Aa]` / `[aa]` marker for a buffer.
pub fn case_indicator(buffer: &str, case: QuickSearchCase) -> &'static str {
    if is_case_sensitive(buffer, case) {
        CASE_SENSITIVE_MARK
    } else {
        CASE_INSENSITIVE_MARK
    }
}

/// The whole status-line fragment, or `None` when no search is running.
pub fn status_label(buffer: &str, case: QuickSearchCase) -> Option<String> {
    if buffer.is_empty() {
        return None;
    }
    Some(format!("search: {buffer} {}", case_indicator(buffer, case)))
}

/// The status-line fragment for one panel, or `None` when it has no search
/// running. This is what the renderer calls.
///
/// A `Ctrl+S` search that has been armed but not yet typed into shows too, as
/// `search: [aa]` - otherwise arming would be invisible.
pub fn panel_status(panel: &Panel, case: QuickSearchCase) -> Option<String> {
    if !panel.quick.is_active() {
        return None;
    }
    let buffer = &panel.quick.buffer;
    let mark = case_indicator(buffer, case);
    Some(if buffer.is_empty() {
        format!("search: {mark}")
    } else {
        format!("search: {buffer} {mark}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_case_matches_the_spec_7_2_example() {
        use QuickSearchCase::Smart;
        use QuickSearchMode::Prefix;
        assert!(quick_match("thorin", "tho", Prefix, Smart));
        assert!(quick_match("Thorin", "tho", Prefix, Smart));
        assert!(quick_match("Thorin", "Tho", Prefix, Smart));
        assert!(!quick_match("thorin", "Tho", Prefix, Smart));
    }

    #[test]
    fn the_marker_and_the_matcher_agree_about_case() {
        // The status line's claim is a statement about `quick_match`, so the
        // two are asked the same question here rather than trusted to have
        // been written from the same rule.
        for (buffer, name) in [("tho", "Thorin"), ("Tho", "Thorin"), ("Tho", "thorin")] {
            let sensitive = case_indicator(buffer, QuickSearchCase::Smart) == CASE_SENSITIVE_MARK;
            let matched = quick_match(
                name,
                buffer,
                QuickSearchMode::Prefix,
                QuickSearchCase::Smart,
            );
            assert_eq!(matched, !sensitive || name.starts_with(buffer));
        }
    }

    #[test]
    fn the_spec_7_2_example_renders_verbatim() {
        assert_eq!(
            status_label("Tho", QuickSearchCase::Smart).as_deref(),
            Some("search: Tho [Aa]")
        );
    }

    #[test]
    fn smart_case_flips_the_marker_when_an_uppercase_character_is_typed() {
        assert_eq!(
            status_label("tho", QuickSearchCase::Smart).as_deref(),
            Some("search: tho [aa]")
        );
        assert_eq!(
            status_label("tho", QuickSearchCase::Sensitive).as_deref(),
            Some("search: tho [Aa]")
        );
        assert_eq!(
            status_label("Tho", QuickSearchCase::Insensitive).as_deref(),
            Some("search: Tho [aa]")
        );
    }

    #[test]
    fn an_empty_buffer_shows_nothing() {
        assert_eq!(status_label("", QuickSearchCase::Smart), None);
    }

    #[test]
    fn an_armed_but_untyped_search_still_shows() {
        use crate::panel::Side;
        use crate::vfs::VfsPath;
        let mut panel = Panel::new(Side::Left, VfsPath::local_root());
        assert_eq!(panel_status(&panel, QuickSearchCase::Smart), None);
        panel.quick.arm();
        assert_eq!(
            panel_status(&panel, QuickSearchCase::Smart).as_deref(),
            Some("search: [aa]")
        );
        panel.quick.push('T');
        assert_eq!(
            panel_status(&panel, QuickSearchCase::Smart).as_deref(),
            Some("search: T [Aa]")
        );
    }
}
