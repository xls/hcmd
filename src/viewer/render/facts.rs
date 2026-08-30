//! A binary shown as what its template says it is.
//!
//! The fallback that gives mode 3 one meaning across every kind of file: it
//! shows **what this is**, rather than what it is made of. JSON becomes a
//! tree, a page becomes its text, and a file with a known format becomes its
//! own description.

use super::super::highlight::SynSlot;
use super::{LineBuf, RenderKind, RenderLine, Rendered};

/// Build the document for `template` against the file's `head`.
///
/// A PNG comes back as `1920 x 1080` and `RGBA` - the same answer the file
/// information dialog gives, from the same summary layer, so the two cannot
/// disagree about what a file is.
///
/// `None` where the template carries no summary of its own. A document of one
/// heading and nothing else is thinner than the text the caller would
/// otherwise show, and the caller's note at least says why.
#[must_use]
pub fn summary_document(
    template: &crate::viewer::template::Template,
    head: &[u8],
) -> Option<Rendered> {
    let facts = crate::viewer::summary::summary(template, head);
    if facts.is_empty() {
        return None;
    }
    let widest = facts
        .iter()
        .map(|line| line.label.chars().count())
        .max()
        .unwrap_or(0);
    let mut heading = LineBuf::default();
    heading.push(
        crate::viewer::summary::heading(template),
        Some(SynSlot::Keyword),
    );
    let mut lines = vec![heading.done(), RenderLine::plain(String::new())];
    for fact in facts {
        let mut line = LineBuf::default();
        let pad = widest.saturating_sub(fact.label.chars().count());
        line.push(&fact.label, Some(SynSlot::Variable));
        line.plain(&" ".repeat(pad.saturating_add(2)));
        line.push(&fact.value, Some(SynSlot::String));
        lines.push(line.done());
    }
    Some(Rendered {
        kind: RenderKind::Summary,
        label: template.name.clone(),
        lines,
    })
}
