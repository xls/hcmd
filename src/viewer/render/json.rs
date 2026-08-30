//! JSON as a tree.
//!
//! One pass over the text, emitting a line per key and per element. Nothing is
//! deserialized into Rust values and no tree is built in memory: the lines
//! *are* the tree, and the only state the walk carries is a stack of the
//! containers it is inside.
//!
//! # What it draws
//!
//! ```text
//! {
//!   name: "holoscommander"
//!   version: "0.1.0"
//!   dependencies: {...} 24 keys
//!   targets: [
//!     {
//!       kind: "bin"
//!       name: "hcmd"
//!     }
//!   ]
//! }
//! ```
//!
//! The commas and the quotes around keys are gone. They are punctuation the
//! format needs and a reader does not, and this is a view of the document
//! rather than of its source - the source is what `1` shows.
//!
//! # Why it is iterative
//!
//! A recursive walk over `[[[[...]]]]` is a stack overflow, and a viewer must
//! open a hostile file rather than abort on one. The container stack here is a
//! `Vec`, so depth costs heap and is bounded by [`MAX_DEPTH`] rather than by
//! the thread's stack.

use super::super::highlight::SynSlot;
use super::json_scan::Scan;
use super::{Fold, LineBuf, RenderLine, indent};

/// How deep a document may nest before the render is refused.
///
/// Deeper than any document written for a person to read, and shallow enough
/// that the indent of the deepest line is still a line. A file past it falls
/// back to text mode with the rest of the unrenderable ones rather than being
/// drawn wrong.
pub const MAX_DEPTH: usize = 256;

/// Which container a frame of the walk is inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Object,
    Array,
}

impl Shape {
    /// The pair of brackets it is written with.
    const fn brackets(self) -> (&'static str, &'static str) {
        match self {
            Self::Object => ("{", "}"),
            Self::Array => ("[", "]"),
        }
    }

    /// What a collapsed one counts.
    const fn unit(self) -> (&'static str, &'static str) {
        match self {
            Self::Object => ("key", "keys"),
            Self::Array => ("item", "items"),
        }
    }
}

/// One open container.
#[derive(Debug)]
struct Frame {
    shape: Shape,
    /// Which line opened it, so the fold can be written when it closes.
    opened_at: usize,
    /// How many members it has had.
    count: usize,
    /// The text of the opening line up to and including the bracket, so the
    /// summary can repeat it: `dependencies: {...} 24 keys`.
    prefix: String,
}

/// The slot a scalar takes.
fn slot_of(raw: &str) -> SynSlot {
    match raw.as_bytes().first() {
        Some(b'"') => SynSlot::String,
        Some(b't' | b'f' | b'n') => SynSlot::Constant,
        _ => SynSlot::Number,
    }
}

/// A string literal with its quotes taken off, escapes left as written.
///
/// Left as written on purpose: `\n` inside a value is shown as the two
/// characters the file holds, because a rendered line that contained a real
/// newline would silently become two rows and the tree would not line up.
fn unquoted(raw: &str) -> &str {
    raw.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(raw)
}

/// `12 keys`, `1 item`, `empty`.
fn summary_count(shape: Shape, count: usize) -> String {
    if count == 0 {
        return "empty".to_string();
    }
    let (one, many) = shape.unit();
    let unit = if count == 1 { one } else { many };
    format!("{count} {unit}")
}

/// Render `text` as a JSON tree, or `None` where it is not JSON.
///
/// Trailing content after the top-level value is refused rather than ignored:
/// a file of one JSON object per line is a real and common thing, and drawing
/// only its first object as if that were the document would be worse than
/// falling back to text.
#[must_use]
pub fn render(text: &str) -> Option<Vec<RenderLine>> {
    let mut scan = Scan::new(text);
    let mut out: Vec<RenderLine> = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();

    scan.skip_space();
    // An empty file is not an empty document; there is nothing to render.
    scan.peek()?;

    loop {
        scan.skip_space();
        // Close as many containers as the text closes here.
        while let Some(frame) = stack.last() {
            let (_, close) = frame.shape.brackets();
            let closer = close.as_bytes().first().copied().unwrap_or(b'}');
            if scan.peek() != Some(closer) {
                break;
            }
            scan.bump();
            let frame = stack.pop()?;
            let depth = stack.len();
            // An empty container is one line. `{}` says everything two lines
            // would, and a tree whose empty objects each cost a row is a tree
            // padded with nothing.
            if out.len().saturating_sub(1) == frame.opened_at {
                if let Some(opener) = out.get_mut(frame.opened_at) {
                    let from = opener.text.len();
                    opener.text.push_str(close);
                    opener.spans.push(super::super::highlight::Span {
                        range: from..opener.text.len(),
                        slot: Some(SynSlot::Punctuation),
                    });
                }
                scan.skip_space();
                scan.eat(b',');
                scan.skip_space();
                continue;
            }
            let mut line = LineBuf::default();
            line.plain(&indent(depth));
            line.push(close, Some(SynSlot::Punctuation));
            out.push(line.done());
            // The region is the lines between the brackets.
            let last = out.len().saturating_sub(1);
            if last > frame.opened_at
                && let Some(opener) = out.get_mut(frame.opened_at)
            {
                let (open, close) = frame.shape.brackets();
                opener.fold = Some(Fold {
                    through: last,
                    summary: format!(
                        "{}{open}...{close} {}",
                        frame.prefix,
                        summary_count(frame.shape, frame.count)
                    ),
                });
            }
            scan.skip_space();
            // A comma after a close belongs to the container outside it.
            scan.eat(b',');
            scan.skip_space();
        }
        if stack.is_empty() && !out.is_empty() {
            // The top-level value is finished. Anything left is a second
            // document, which this is not a view of.
            scan.skip_space();
            return scan.peek().is_none().then_some(out);
        }

        let depth = stack.len();
        if depth >= MAX_DEPTH {
            return None;
        }
        let mut line = LineBuf::default();
        line.plain(&indent(depth));

        // Inside an object every member is named.
        if stack.last().map(|f| f.shape) == Some(Shape::Object) {
            let key = scan.string()?;
            line.push(unquoted(key), Some(SynSlot::Variable));
            line.plain(": ");
            scan.skip_space();
            if !scan.eat(b':') {
                return None;
            }
            scan.skip_space();
        }
        if let Some(frame) = stack.last_mut() {
            frame.count = frame.count.saturating_add(1);
        }

        match scan.peek()? {
            open @ (b'{' | b'[') => {
                scan.bump();
                let shape = if open == b'{' {
                    Shape::Object
                } else {
                    Shape::Array
                };
                let (bracket, _) = shape.brackets();
                let prefix = line.as_text().to_string();
                line.push(bracket, Some(SynSlot::Punctuation));
                out.push(line.done());
                stack.push(Frame {
                    shape,
                    opened_at: out.len().saturating_sub(1),
                    count: 0,
                    prefix,
                });
            }
            b'"' => {
                let raw = scan.string()?;
                line.push(raw, Some(SynSlot::String));
                out.push(line.done());
                scan.skip_space();
                scan.eat(b',');
            }
            _ => {
                let raw = scan.literal()?;
                line.push(raw, Some(slot_of(raw)));
                out.push(line.done());
                scan.skip_space();
                scan.eat(b',');
            }
        }
    }
}

#[cfg(test)]
#[path = "json_tests.rs"]
mod tests;
