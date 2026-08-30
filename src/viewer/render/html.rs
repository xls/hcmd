//! HTML as the text it renders to.
//!
//! A tag-aware extractor rather than a parser: it walks the markup once,
//! keeping the block structure and dropping everything that is markup. No DOM
//! is built, nothing is validated, and malformed markup produces a worse
//! rendering rather than an error - which is the right trade for a viewer,
//! where the file is somebody else's and is opened to find out what is in it.
//!
//! A parser crate was measured against the release binary's ceiling and did
//! not fit; it would also have been the wrong shape, since a DOM is a tree
//! built to be queried and this needs one pass that emits lines.
//!
//! # What survives
//!
//! * **Headings** stand out and carry their level.
//! * **Paragraphs, list items, table rows and block quotes** each start a line.
//! * **Links** keep their text, with the target after it.
//! * **`script` and `style`** are dropped whole - contents included, which is
//!   the point: a page's JavaScript is not its text.
//! * **Entities** are decoded for the handful that matter, so `&amp;` reads as
//!   `&` rather than as itself.

use super::super::highlight::SynSlot;
use super::entities::decode;
use super::{LineBuf, RenderLine};

/// Tags whose contents are not text and are dropped entirely.
const DROPPED: [&str; 4] = ["script", "style", "head", "template"];

/// Tags that start a new line before and after themselves.
const BLOCKS: [&str; 26] = [
    "p",
    "div",
    "section",
    "article",
    "header",
    "footer",
    "main",
    "nav",
    "aside",
    "ul",
    "ol",
    "li",
    "dl",
    "dt",
    "dd",
    "table",
    "tr",
    "blockquote",
    "pre",
    "form",
    "figure",
    "figcaption",
    "hr",
    "br",
    "h1",
    "h2",
];

/// How a heading level is drawn.
fn heading_prefix(level: usize) -> String {
    match level {
        1 => String::new(),
        2 => String::new(),
        _ => "  ".repeat(level.saturating_sub(2)),
    }
}

/// One tag, as the walk sees it.
#[derive(Debug)]
struct Tag<'a> {
    name: String,
    closing: bool,
    /// The raw attribute text, for the one attribute anything here reads.
    attrs: &'a str,
}

impl Tag<'_> {
    /// The value of `name`, unquoted, if the tag has it.
    fn attr(&self, name: &str) -> Option<&str> {
        let at = self.attrs.to_ascii_lowercase().find(name)?;
        let rest = self
            .attrs
            .get(at.saturating_add(name.len())..)?
            .trim_start();
        let rest = rest.strip_prefix('=')?.trim_start();
        let quote = rest.chars().next()?;
        if quote == '"' || quote == '\'' {
            let body = rest.get(1..)?;
            return body.find(quote).and_then(|end| body.get(..end));
        }
        Some(rest.split_whitespace().next().unwrap_or_default())
    }

    /// The heading level, for `h1` to `h6`.
    fn heading_level(&self) -> Option<usize> {
        let rest = self.name.strip_prefix('h')?;
        let level: usize = rest.parse().ok()?;
        (1..=6).contains(&level).then_some(level)
    }
}

/// Read the tag beginning at `at`, and where it ends.
fn tag_at(text: &str, at: usize) -> Option<(Tag<'_>, usize)> {
    let rest = text.get(at.saturating_add(1)..)?;
    let end = rest.find('>')?;
    let body = rest.get(..end)?;
    let body = body.strip_suffix('/').unwrap_or(body);
    let closing = body.starts_with('/');
    let body = body.strip_prefix('/').unwrap_or(body);
    let split = body
        .find(|c: char| c.is_ascii_whitespace())
        .unwrap_or(body.len());
    let name = body.get(..split)?.to_ascii_lowercase();
    Some((
        Tag {
            name,
            closing,
            attrs: body.get(split..).unwrap_or_default(),
        },
        at.saturating_add(end).saturating_add(2),
    ))
}

/// The document being assembled, one block at a time.
#[derive(Default)]
struct Page {
    out: Vec<RenderLine>,
    /// The block being filled.
    line: LineBuf,
    /// The heading level the current block is, when it is one.
    heading: Option<usize>,
}

impl Page {
    /// True at the top of the page and after a blank line, which are the two
    /// places another blank line would be one too many.
    fn ends_blank(&self) -> bool {
        self.out.last().is_none_or(|line| line.text.is_empty())
    }

    /// Finish the block being built, if it has anything in it.
    fn flush(&mut self) {
        let built = std::mem::take(&mut self.line);
        let level = self.heading.take();
        if built.as_text().trim().is_empty() {
            return;
        }
        let done = built.done();
        // A heading gets a blank line before it and its level's indent, which
        // is what makes the structure of a page visible at a glance.
        if let Some(level) = level {
            if !self.ends_blank() {
                self.out.push(RenderLine::plain(String::new()));
            }
            let mut head = LineBuf::default();
            head.plain(&heading_prefix(level));
            let slot = if level <= 2 {
                SynSlot::Keyword
            } else {
                SynSlot::Type
            };
            head.push(done.text.trim(), Some(slot));
            self.out.push(head.done());
            return;
        }
        self.out.push(done);
    }

    /// End the block and leave a blank line, for the tags that separate.
    fn break_block(&mut self) {
        self.flush();
        if !self.ends_blank() {
            self.out.push(RenderLine::plain(String::new()));
        }
    }

    /// Add text to the block being built, collapsing runs of whitespace.
    ///
    /// Collapsed because HTML says so: the newlines and the indentation in the
    /// source are formatting of the *markup*, and a renderer that kept them
    /// would be showing the author's editor settings rather than the page.
    fn text(&mut self, raw: &str, slot: Option<SynSlot>) {
        let decoded = decode(raw);
        let mut first = true;
        for word in decoded.split_whitespace() {
            // A space between words, and never one after a marker that
            // already put its own: `  * ` followed by a word is one space, not
            // two.
            let started = self.line.as_text();
            if (!first || !started.is_empty()) && !started.ends_with(' ') {
                self.line.plain(" ");
            }
            self.line.push(word, slot);
            first = false;
        }
    }
}

/// Render `text` as HTML.
///
/// Never fails, for the same reason the Markdown renderer does not: there is
/// no input this cannot walk, only input it renders poorly.
#[must_use]
pub fn render(text: &str) -> Vec<RenderLine> {
    let mut page = Page::default();
    let mut at = 0_usize;
    let mut plain_from = 0_usize;
    let mut link: Option<String> = None;

    while at < text.len() {
        let Some(next) = text.get(at..).and_then(|rest| rest.find('<')) else {
            break;
        };
        let open = at.saturating_add(next);
        let Some((tag, end)) = tag_at(text, open) else {
            at = open.saturating_add(1);
            continue;
        };
        if let Some(run) = text.get(plain_from..open) {
            page.text(run, None);
        }
        at = end;
        plain_from = end;

        if !tag.closing && DROPPED.contains(&tag.name.as_str()) {
            page.flush();
            // Jump straight to the closing tag by name rather than carrying on
            // through the general tag scan. A script's body is not markup: an
            // `if (a < b)` in it looks exactly like the start of a tag, and
            // scanning for the next `>` swallowed the `</script>` along with
            // the rest of the page. Searching for the one string that really
            // ends the element cannot do that.
            let close = format!("</{}", tag.name);
            at = match text
                .get(end..)
                .map(str::to_ascii_lowercase)
                .and_then(|rest| rest.find(&close).map(|found| end.saturating_add(found)))
            {
                Some(found) => match text.get(found..).and_then(|rest| rest.find('>')) {
                    Some(shut) => found.saturating_add(shut).saturating_add(1),
                    // An unterminated close: the rest of the file is inside it.
                    None => text.len(),
                },
                // Never closed at all, which a truncated page really is.
                None => text.len(),
            };
            plain_from = at;
            continue;
        }
        if let Some(level) = tag.heading_level() {
            page.break_block();
            if !tag.closing {
                page.heading = Some(level);
            }
            continue;
        }
        match tag.name.as_str() {
            "li" if !tag.closing => {
                page.flush();
                page.line.push("  * ", Some(SynSlot::Punctuation));
            }
            // A list item ends its own line without a blank one after it: a
            // list is a block, its items are rows of that block.
            "li" if tag.closing => page.flush(),
            // Two spaces between cells, and none before the first: a row is a
            // line of text, not an indented one.
            "td" | "th" if !tag.closing && !page.line.as_text().is_empty() => {
                page.line.plain("  ");
            }
            "td" | "th" if tag.closing => {}
            "a" if !tag.closing => link = tag.attr("href").map(str::to_string),
            "a" if tag.closing => {
                if let Some(href) = link.take().filter(|h| !h.is_empty()) {
                    page.line.plain(" (");
                    page.line.push(&href, Some(SynSlot::Comment));
                    page.line.plain(")");
                }
            }
            name if BLOCKS.contains(&name) => page.break_block(),
            _ => {}
        }
    }
    if let Some(run) = text.get(plain_from..) {
        page.text(run, None);
    }
    page.flush();
    // Runs of blank lines are one blank line: a page whose markup nests ten
    // divs deep must not open with ten empty rows.
    let mut out: Vec<RenderLine> = Vec::with_capacity(page.out.len());
    for line in page.out {
        if line.text.is_empty() && out.last().is_some_and(|l| l.text.is_empty()) {
            continue;
        }
        out.push(line);
    }
    while out.last().is_some_and(|l| l.text.is_empty()) {
        out.pop();
    }
    out
}

#[cfg(test)]
#[path = "html_tests.rs"]
mod tests;
