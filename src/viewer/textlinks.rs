//! Web links in plain text: mode 1's answer to mode 3's [`super::links`].
//!
//! A text file has no link data, only bytes, so a link here is a `http://` or
//! `https://` token wherever it happens to sit. `Tab` finds the next one with
//! the find engine's bounded reads - the same machinery `n` uses, with its own
//! matcher so the user's search is left alone - and puts the byte cursor on
//! it; `Enter` and `Shift+Enter` act on whatever token the cursor is standing
//! in. Nothing is scanned whole: a 40 GB log is read a window at a time, and a
//! link out of reach of one step is reported as not found rather than hunted
//! for.

use super::Viewer;
use super::find::{self, FindKind, FindQuery, Found, Matcher};
use super::source::WindowLen;
use crate::config::ViewerMode;
use crate::error::Result;

/// How far around the cursor a link may extend, in bytes either way. Longer
/// than any sane URL and shorter than a window worth worrying about.
const REACH: u64 = 2048;

/// Punctuation a sentence puts after a link that is not part of it.
const TRAILING: [char; 6] = ['.', ',', ';', ':', '!', '?'];

/// Is this a byte a URL may be made of? The token ends at whitespace or at a
/// bracket or quote that would have wrapped the link.
fn is_url_byte(b: u8) -> bool {
    !b.is_ascii_whitespace()
        && !matches!(
            b,
            b'<' | b'>' | b'"' | b'\'' | b'`' | b'(' | b')' | b'[' | b']' | b'{' | b'}'
        )
}

/// The web link whose token covers byte `at` of `bytes`, as (its start within
/// `bytes`, the link). `None` when the token there is not a link.
///
/// The scheme may sit inside the token (`see:https://x`), in which case the
/// link starts at the scheme, and trailing sentence punctuation is dropped.
pub fn url_span_at(bytes: &[u8], at: usize) -> Option<(usize, String)> {
    if at >= bytes.len() {
        return None;
    }
    let mut start = at;
    while start > 0
        && bytes
            .get(start.saturating_sub(1))
            .is_some_and(|&b| is_url_byte(b))
    {
        start = start.saturating_sub(1);
    }
    let mut end = at;
    while bytes.get(end).is_some_and(|&b| is_url_byte(b)) {
        end = end.saturating_add(1);
    }
    let token = std::str::from_utf8(bytes.get(start..end)?).ok()?;
    let scheme_at = token.find("https://").or_else(|| token.find("http://"))?;
    let url = token.get(scheme_at..)?.trim_end_matches(TRAILING);
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    if rest.is_empty() {
        return None;
    }
    Some((start.saturating_add(scheme_at), url.to_string()))
}

impl Viewer {
    /// The web link under the byte cursor in text mode, as (its offset in the
    /// file, the link).
    pub(super) fn url_span_under_cursor(&mut self) -> Option<(u64, String)> {
        if self.mode != ViewerMode::Text {
            return None;
        }
        let at = self.cursor;
        let from = at.saturating_sub(REACH);
        let len = usize::try_from(REACH.saturating_mul(2)).unwrap_or(4096);
        let window = self.source.read_window(from, WindowLen::new(len)).ok()?;
        let rel = usize::try_from(at.saturating_sub(window.at())).ok()?;
        url_span_at(window.bytes(), rel)
            .map(|(start, url)| (window.at().saturating_add(start as u64), url))
    }

    /// The web link under the byte cursor, in text mode.
    pub fn url_under_cursor(&mut self) -> Option<String> {
        self.url_span_under_cursor().map(|(_, url)| url)
    }

    /// Put the cursor on the next web link after it (or the previous one
    /// before it) and say which. `Ok(None)` when none is within one search
    /// step's reach in that direction.
    ///
    /// Searches for `://` and keeps only the hits that read as a web link, so
    /// a `file://` or a stray `://` in prose is stepped over. A handful of
    /// tries is plenty; the read budget bounds each one. The search starts
    /// past the link the cursor already stands in, so `Tab` on a link goes to
    /// the *next* one rather than finding its own `://` again.
    pub fn step_text_link(&mut self, forward: bool) -> Result<Option<String>> {
        let query = FindQuery {
            input: "://".to_string(),
            kind: FindKind::Text,
            ..FindQuery::default()
        };
        let Ok(matcher) = Matcher::compile(&query, self.encoding, false) else {
            return Ok(None);
        };
        let mut from = match (self.url_span_under_cursor(), forward) {
            (Some((start, url)), true) => start.saturating_add(url.len() as u64),
            (Some((start, _)), false) => start,
            (None, true) => self.cursor.saturating_add(1),
            (None, false) => self.cursor,
        };
        for _ in 0..8 {
            let found = if forward {
                find::find_forward(&mut self.source, &matcher, from, find::FIND_READ_BUDGET)?
            } else {
                find::find_backward(&mut self.source, &matcher, from, find::FIND_READ_BUDGET)?
            };
            let Found::Hit(at) = found else {
                return Ok(None);
            };
            let win_from = at.saturating_sub(64);
            let len = usize::try_from(REACH.saturating_add(64)).unwrap_or(2112);
            let window = self.source.read_window(win_from, WindowLen::new(len))?;
            let rel = usize::try_from(at.saturating_sub(window.at())).unwrap_or(0);
            if let Some((start, url)) = url_span_at(window.bytes(), rel) {
                let offset = window.at().saturating_add(start as u64);
                self.goto_offset(offset)?;
                return Ok(Some(url));
            }
            from = if forward { at.saturating_add(1) } else { at };
        }
        Ok(None)
    }
}

#[cfg(test)]
#[path = "textlinks_tests.rs"]
mod tests;
