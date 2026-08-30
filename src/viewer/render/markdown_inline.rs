//! Markdown's inline markers: emphasis, code spans and links.
//!
//! The markers go and the text stays. This is the half of Markdown that does
//! not change a document's shape, which is why it is here rather than in the
//! block walk beside it.

use super::super::highlight::SynSlot;
use super::LineBuf;

/// Strip inline emphasis, code and link syntax from one line of prose.
///
/// The markers go and the text stays. A link becomes its text followed by its
/// target in brackets, because the target is the half a reader cannot get back
/// any other way and dropping it silently would lose the document's content
/// rather than its notation.
pub(crate) fn inline(line: &mut LineBuf, text: &str) {
    let bytes = text.as_bytes();
    let mut at = 0_usize;
    let mut plain_from = 0_usize;
    while at < bytes.len() {
        let here = bytes.get(at).copied().unwrap_or(0);
        // A link: `[text](target)`.
        if here == b'['
            && let Some((label, target, end)) = link_at(text, at)
        {
            flush(line, text, plain_from, at);
            line.push(label, Some(SynSlot::Function));
            if !target.is_empty() {
                line.plain(" (");
                line.push(target, Some(SynSlot::Comment));
                line.plain(")");
            }
            at = end;
            plain_from = end;
            continue;
        }
        // Emphasis and inline code, whose markers are runs of one character.
        // An underscore inside a word is not emphasis, which is what keeps
        // `snake_case_name` from rendering as `snakecasename`. Markdown says
        // so and every writer of one relies on it.
        if matches!(here, b'*' | b'_' | b'`')
            && !(here == b'_' && at > 0 && word_byte(bytes.get(at.saturating_sub(1)).copied()))
            && let Some((body, end)) = marked_at(text, at, here)
        {
            flush(line, text, plain_from, at);
            let slot = if here == b'`' {
                SynSlot::String
            } else {
                SynSlot::Keyword
            };
            line.push(body, Some(slot));
            at = end;
            plain_from = end;
            continue;
        }
        at = at.saturating_add(1);
    }
    flush(line, text, plain_from, bytes.len());
}

/// Is this byte part of a word, for the underscore rule above?
///
/// `None` is the start of the line, which is not inside a word.
fn word_byte(byte: Option<u8>) -> bool {
    byte.is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Append the untouched run between two markers.
pub(crate) fn flush(line: &mut LineBuf, text: &str, from: usize, to: usize) {
    if let Some(run) = text.get(from..to) {
        line.plain(run);
    }
}

/// A `[label](target)` beginning at `at`, and where it ends.
fn link_at(text: &str, at: usize) -> Option<(&str, &str, usize)> {
    let rest = text.get(at.saturating_add(1)..)?;
    let close = rest.find(']')?;
    let label = rest.get(..close)?;
    let after = rest.get(close.saturating_add(1)..)?;
    if !after.starts_with('(') {
        // A bare `[text]` reference: keep the text, drop the brackets.
        return Some((label, "", at.saturating_add(close).saturating_add(2)));
    }
    let end = after.find(')')?;
    let target = after.get(1..end)?;
    Some((
        label,
        target,
        at.saturating_add(close)
            .saturating_add(end)
            .saturating_add(3),
    ))
}

/// A run marked with one or two `mark` characters, and where it ends.
fn marked_at(text: &str, at: usize, mark: u8) -> Option<(&str, usize)> {
    let marker = if text
        .as_bytes()
        .get(at.saturating_add(1))
        .is_some_and(|b| *b == mark)
    {
        2
    } else {
        1
    };
    let open = at.saturating_add(marker);
    let rest = text.get(open..)?;
    let pattern = String::from_utf8(vec![mark; marker]).ok()?;
    let close = rest.find(&pattern)?;
    if close == 0 {
        return None;
    }
    let body = rest.get(..close)?;
    Some((body, open.saturating_add(close).saturating_add(marker)))
}
