//! Markdown rendered rather than spelled.
//!
//! Block structure, which is the half of Markdown that changes how a document
//! is *shaped*: headings, lists, block quotes, fenced code, rules and
//! paragraphs. Inline emphasis is stripped of its markers and coloured, so
//! `**important**` reads as `important` in the emphasis colour rather than as
//! four asterisks a person has to look past.
//!
//! ```text
//! ══ Installing ══
//!
//! Run the installer, then
//!
//!   * check the version
//!   * put it on the PATH
//!
//!     cargo install hcmd
//! ```
//!
//! # What it deliberately does not do
//!
//! No reflowing. A paragraph is left as the lines the file has, because the
//! viewer already wraps to the terminal and a renderer that also wrapped would
//! be wrapping twice, at two widths, neither of them the one on screen.
//!
//! No tables. A table needs a width to lay out against and this runs before
//! the terminal's width is known; the rows are shown as they are written,
//! which is legible and honest, rather than half-aligned against a guess.

use super::super::highlight::SynSlot;
use super::markdown_inline::inline;
use super::{LineBuf, RenderLine};

/// The rule drawn for a thematic break and around a top-level heading.
const RULE: char = '=';

/// How wide a heading's decoration is, at most.
const RULE_WIDTH: usize = 60;

/// A heading, with its level shown by decoration rather than by hashes.
fn heading(level: usize, text: &str) -> Vec<RenderLine> {
    let mut line = LineBuf::default();
    match level {
        1 => {
            line.push(&text.to_uppercase(), Some(SynSlot::Keyword));
        }
        2 => {
            line.push(text, Some(SynSlot::Keyword));
        }
        _ => {
            line.plain(&"  ".repeat(level.saturating_sub(2)));
            line.push(text, Some(SynSlot::Type));
        }
    }
    let width = line.as_text().chars().count().min(RULE_WIDTH);
    let mut out = vec![line.done()];
    // A rule under the two levels that carry a document's structure, and
    // nothing under the rest: a rule under every `####` would be more
    // decoration than document.
    if level <= 2 {
        let mut rule = LineBuf::default();
        let glyph = if level == 1 { RULE } else { '-' };
        rule.push(
            &glyph.to_string().repeat(width.max(3)),
            Some(SynSlot::Punctuation),
        );
        out.push(rule.done());
    }
    out
}

/// Render `text` as Markdown.
///
/// Never fails: every text file is valid Markdown, which is the format's whole
/// design, so there is no parse to refuse. A file with no markup in it comes
/// back as its own paragraphs.
#[must_use]
pub fn render(text: &str) -> Vec<RenderLine> {
    let mut out: Vec<RenderLine> = Vec::new();
    let mut fence: Option<String> = None;

    for raw in text.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim_start();

        // Inside a fenced block nothing is markup, including the things that
        // look like it. That is what a code block is for.
        if let Some(marker) = fence.as_ref() {
            if trimmed.starts_with(marker.as_str()) {
                fence = None;
                continue;
            }
            let mut code = LineBuf::default();
            code.plain("    ");
            code.push(line, Some(SynSlot::String));
            out.push(code.done());
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("```").or(trimmed.strip_prefix("~~~")) {
            let marker = if trimmed.starts_with("```") {
                "```"
            } else {
                "~~~"
            };
            fence = Some(marker.to_string());
            let language = rest.trim();
            if !language.is_empty() {
                let mut head = LineBuf::default();
                head.push(&format!("    [{language}]"), Some(SynSlot::Comment));
                out.push(head.done());
            }
            continue;
        }

        if trimmed.is_empty() {
            out.push(RenderLine::plain(String::new()));
            continue;
        }

        // A thematic break.
        if trimmed.len() >= 3
            && trimmed
                .chars()
                .all(|c| c == '-' || c == '*' || c == '_' || c == ' ')
            && trimmed.chars().any(|c| c != ' ')
        {
            let mut rule = LineBuf::default();
            rule.push(
                &RULE.to_string().repeat(RULE_WIDTH),
                Some(SynSlot::Punctuation),
            );
            out.push(rule.done());
            continue;
        }

        // A heading.
        if let Some(hashes) = trimmed.strip_prefix('#') {
            let level = 1 + hashes.chars().take_while(|c| *c == '#').count();
            let body = trimmed.trim_start_matches('#').trim();
            if level <= 6 && !body.is_empty() {
                out.extend(heading(level, body));
                continue;
            }
        }

        // A block quote, however deeply nested.
        if trimmed.starts_with('>') {
            let depth = trimmed
                .chars()
                .take_while(|c| *c == '>' || *c == ' ')
                .count();
            let body = trimmed.get(depth..).unwrap_or_default();
            let mut quote = LineBuf::default();
            quote.push("  | ", Some(SynSlot::Punctuation));
            inline(&mut quote, body);
            out.push(quote.done());
            continue;
        }

        // A list item, bullet or numbered. The file's own indentation decides
        // the nesting, which is what Markdown says and what the writer saw.
        let lead = line.len().saturating_sub(trimmed.len());
        if let Some(body) = bullet_body(trimmed) {
            let mut item = LineBuf::default();
            item.plain(&" ".repeat(lead.saturating_add(2)));
            item.push("* ", Some(SynSlot::Punctuation));
            inline(&mut item, body);
            out.push(item.done());
            continue;
        }
        if let Some((number, body)) = numbered_body(trimmed) {
            let mut item = LineBuf::default();
            item.plain(&" ".repeat(lead.saturating_add(2)));
            item.push(&format!("{number}. "), Some(SynSlot::Punctuation));
            inline(&mut item, body);
            out.push(item.done());
            continue;
        }

        // An indented code block.
        if lead >= 4 {
            let mut code = LineBuf::default();
            code.plain("    ");
            code.push(trimmed, Some(SynSlot::String));
            out.push(code.done());
            continue;
        }

        let mut prose = LineBuf::default();
        inline(&mut prose, line);
        out.push(prose.done());
    }
    out
}

/// The text of a bullet item, or `None` where the line is not one.
fn bullet_body(trimmed: &str) -> Option<&str> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(body) = trimmed.strip_prefix(marker) {
            return Some(body);
        }
    }
    None
}

/// The number and text of an ordered item, or `None` where it is not one.
fn numbered_body(trimmed: &str) -> Option<(&str, &str)> {
    let digits = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .filter(|at| *at > 0)?;
    let number = trimmed.get(..digits)?;
    let rest = trimmed.get(digits..)?;
    let body = rest.strip_prefix(". ").or(rest.strip_prefix(") "))?;
    Some((number, body))
}

#[cfg(test)]
#[path = "markdown_tests.rs"]
mod tests;
