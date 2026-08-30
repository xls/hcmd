//! Why mode 3 will not show a file, in words a person can act on.
//!
//! Three refusals and one rule between them: each says what it could not do
//! and, where a setting is what would change the answer, names the setting.
//! A view that quietly showed something else instead would be worse than any
//! of them.

use super::render::RenderKind;

/// What mode 3 says when it will not render a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderRefusal {
    /// Nothing here renders this kind of file.
    NoRenderer,
    /// The file is over `viewer.render.max_size`.
    TooBig {
        /// The file's length.
        len: u64,
        /// The configured ceiling.
        limit: u64,
    },
    /// It has a renderer and did not parse as that format.
    NotThatFormat(RenderKind),
}

impl RenderRefusal {
    /// The sentence the status line shows, which names the setting where the
    /// setting is what to change.
    #[must_use]
    pub fn message(&self, name: &str) -> String {
        match self {
            Self::NoRenderer => format!(
                "{name}: nothing renders this; showing it as text. Mode 3 knows JSON, HTML and Markdown"
            ),
            Self::TooBig { len, limit } => format!(
                "{name}: {} is over the {} viewer.render.max_size ceiling - mode 3 has to read the whole file, so it will not open this one",
                crate::panel::format::human_size(*len),
                crate::panel::format::human_size(*limit),
            ),
            Self::NotThatFormat(kind) => format!(
                "{name}: does not parse as {}; showing it as text",
                kind.label()
            ),
        }
    }
}
