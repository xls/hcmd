//! What the dialog will not let a search start with.
//!
//! Every field that can be typed into can be typed into wrongly, and this is
//! the one place that decides which of those the dialog refuses. Refusing is
//! not the same as being empty: an empty size bound and a size bound of
//! `twelve` are different answers, and only the second is an error.
//!
//! A message here is what the dialog shows beside the field, so it is written
//! to be read at the width the dialog has rather than as a diagnostic.

use super::*;

impl FindDialog {
    /// The fields that are not a **value** at all, with the tab that holds
    /// them.
    ///
    /// [`Query::compile`] is the one place a search is refused, and it can
    /// only object to values: `eight megabytes` in a size field never becomes
    /// a [`SizeRange`] for it to look at. This is that gap and nothing more -
    /// every refusal `compile` can make is left to `compile`, so one wording
    /// cannot drift from the other.
    ///
    /// Refused here rather than after the dialog closes so the typo is still
    /// on the screen to correct - the rule, applied to this
    /// dialog.
    fn parse_errors(&self) -> Option<(TabKind, String)> {
        if self.roots.iter().all(|r| r.trim().is_empty()) {
            return Some((TabKind::General, "a search needs somewhere to start".into()));
        }
        let start_text = self.start.to_string();
        for text in &self.roots {
            let text = text.trim();
            if !text.is_empty()
                && text != start_text
                && let Err(err) = crate::panel::goto::expand(text, self.start.local_path())
            {
                return Some((TabKind::General, format!("Search in: {err}")));
            }
        }
        if let Err(err) = parse_size(self.size_min.text()) {
            return Some((TabKind::Advanced, format!("Size at least: {err}")));
        }
        if let Err(err) = parse_size(self.size_max.text()) {
            return Some((TabKind::Advanced, format!("Size at most: {err}")));
        }
        match self.date_choice {
            DateChoice::Any => {}
            DateChoice::Between => {
                for (text, edge, label) in [
                    (self.after.text(), DayEdge::Start, "after"),
                    (self.before.text(), DayEdge::End, "before"),
                ] {
                    if let Err(err) = check_date(text, edge, label) {
                        return Some((TabKind::Advanced, err));
                    }
                }
            }
            DateChoice::Newer => {
                let text = self.days.text().trim();
                if text.parse::<u32>().is_err() {
                    return Some((
                        TabKind::Advanced,
                        format!("newer than: {text:?} is not a number of days"),
                    ));
                }
            }
        }
        None
    }

    /// Which tab a [`Query::compile`] refusal is about.
    ///
    /// The message is `compile`'s, because there is one wording for a refused
    /// search; the tab is the dialog's, because only the dialog knows which
    /// control holds the value that was refused.
    fn tab_for(query: &Query) -> TabKind {
        let size = matches!((query.size.min, query.size.max), (Some(min), Some(max)) if min > max);
        let date = match query.date {
            DateRange::Any => false,
            DateRange::Between { after, before } => {
                matches!((after, before), (Some(a), Some(b)) if a > b)
            }
            DateRange::NewerThanDays(days) => days == 0,
        };
        if size || date {
            TabKind::Advanced
        } else {
            TabKind::General
        }
    }

    /// Show a refusal, on the tab that holds it.
    fn refuse(&mut self, tab: TabKind, message: String) -> DialogOutcome {
        self.set_tab(tab);
        self.error = Some(message);
        DialogOutcome::Consumed
    }

    /// `Start search`.
    pub(super) fn accept(&mut self) -> DialogOutcome {
        if let Some((tab, message)) = self.parse_errors() {
            return self.refuse(tab, message);
        }
        let query = self.query();
        // `Query::compile` is the one place a search is refused, so the
        // dialog's message and the engine's cannot differ.
        if let Err(err) = query.compile() {
            return self.refuse(Self::tab_for(&query), err.to_string());
        }
        DialogOutcome::Accept(DialogResult::Find(Box::new(FindAnswer {
            query,
            saved: self.saved_dirty.then(|| self.saved.clone()),
            tab: self.tab.index(),
        })))
    }
}

/// The mask a `Query` gets from the field: `*` and an empty field are the same
/// question, and `Query` spells it empty.
pub(super) fn mask_text(text: &str) -> String {
    let text = text.trim();
    if text == "*" {
        String::new()
    } else {
        text.to_string()
    }
}

/// The index of a depth in [`Depth::CHOICES`].
pub(super) fn depth_index(depth: Depth) -> usize {
    Depth::CHOICES
        .iter()
        .position(|d| *d == depth)
        .unwrap_or_else(default_depth_index)
}

/// The dropdown's default: `all (unlimited depth)`.
pub(super) fn default_depth_index() -> usize {
    Depth::CHOICES
        .iter()
        .position(|d| *d == Depth::Unlimited)
        .unwrap_or(0)
}

/// A size field's contents. `Ok(None)` is an empty field, which is "no bound".
pub(super) fn parse_size(text: &str) -> Result<Option<u64>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    ByteSize::parse(text).map(|b| Some(b.bytes()))
}

/// A date field's contents, with the reason when it is not a date.
pub(super) fn check_date(
    text: &str,
    edge: DayEdge,
    label: &str,
) -> Result<Option<std::time::SystemTime>, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    parse_date(trimmed, edge)
        .map(Some)
        .ok_or_else(|| format!("{label}: {trimmed:?} is not a date (YYYY-MM-DD)"))
}

/// A byte bound as the Advanced tab spells it.
pub(super) fn spell_size(bytes: Option<u64>) -> String {
    bytes.map(|b| ByteSize(b).to_string()).unwrap_or_default()
}

/// `$HOME` folded back to `~`, for the root list line (the grammar).
pub(super) fn fold_home(text: &str) -> String {
    let Ok(home) = crate::config::paths::home_dir() else {
        return text.to_string();
    };
    let home = home.to_string_lossy().into_owned();
    if home.is_empty() || home == "/" {
        return text.to_string();
    }
    match text.strip_prefix(&home) {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        Some(_) | None => text.to_string(),
    }
}
