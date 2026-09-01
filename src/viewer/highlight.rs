//! Syntax highlighting.
//!
//! > the pick is **`syntect` + `two-face`** … Critically for, it is
//! > **line-oriented**: a `ParseState` advances one line at a time, so
//! > highlighting can start from a checkpoint and cover just the visible
//! > window. That matches the streaming model exactly.
//!
//! the v0.4 line says "tree-sitter highlighting". That phrase is stale and
//! the design settles it the other way, in writing, with the reason: a
//! whole-buffer parser is the wrong shape for a streaming viewer. This
//! module is `syntect`.
//!
//! # Styles are mapped onto theme slots, not onto syntect's themes
//!
//! > Map syntect styles onto theme slots, not onto syntect's own themes,
//! > so the blue theme controls highlighting and a 16-color terminal degrades.
//! > Write that mapping in-house.
//!
//! So nothing here ever loads a syntect `Theme`. The parser is driven for its
//! **scopes** - `keyword.control.rust`, `string.quoted.double` - and
//! [`SynSlot::for_scope`] turns a scope into one of the `syn.*`
//! slots, through the ordered table in [`SELECTORS`]. A 16-color session then
//! quantizes those slots like any other colour.
//!
//! # Where the streaming model shows up
//!
//! A `ParseState` is a *resumable* position in a file, so highlighting the
//! visible window means starting from a saved state and parsing forward. This
//! module owns two halves of that mechanic:
//!
//! * [`Highlighter`] is the resumable position itself. **Cloning one is taking
//!   a checkpoint.**
//! * [`Checkpoints`] is a sparse, capped, self-decimating store of them by byte
//!   offset - the same shape as [`crate::viewer::index::LineIndex`], and for
//!   the same reason: scrolling into the middle of a large file should resume
//!   from the nearest checkpoint rather than re-parse from the top, and the
//!   store must cost the same for a 40 GB file as for a 4 KB one.
//!
//! Highlighting is skipped entirely above `viewer.highlight.max_size`
//! - the file still opens, it is just plain.
//!
//! # Nothing here can hang the UI thread
//!
//! `Viewer::layout` runs on the event-loop thread, so a pathological file must
//! not be able to stop the frame. Three bounds, all local to this module:
//!
//! 1. `fancy-regex` carries a backtrack limit (one million steps by default)
//!    and syntect's fancy backend turns the resulting error into "no match", so
//!    catastrophic backtracking terminates instead of spinning.
//! 2. A line longer than [`MAX_HIGHLIGHT_LINE`] is parsed only up to that
//!    prefix; the tail is drawn plain.
//! 3. A [`Highlighter`] carries a wall-clock [`PARSE_BUDGET`] for the run of
//!    lines it is asked about. Once it is spent the highlighter *stalls*: every
//!    further line comes back as one plain span, drawn instantly. Resuming from
//!    a checkpoint hands out a fresh budget, so the next frame tries again.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

/// One of the `syn.*` theme slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SynSlot {
    /// `syn.keyword`.
    Keyword,
    /// `syn.string`.
    String,
    /// `syn.comment`.
    Comment,
    /// `syn.type`.
    Type,
    /// `syn.function`.
    Function,
    /// `syn.number`.
    Number,
    /// `syn.constant`.
    Constant,
    /// `syn.operator`.
    Operator,
    /// `syn.punctuation`.
    Punctuation,
    /// `syn.variable`.
    Variable,
    /// `diff.added`: a line the right-hand side has and the left does not.
    ///
    /// The last four are not syntax, and they live here because this is the
    /// slot type a [`Span`] carries and a rendered diff paints its lines with
    /// spans like every other rendered document. Keeping them out would mean a
    /// second colour path through the renderer for one kind of document.
    DiffAdded,
    /// `diff.removed`: a line the left-hand side has and the right does not.
    DiffRemoved,
    /// `diff.header`: the `@@` line, and the two file names above it.
    DiffHeader,
    /// `diff.marker`: a collapsed run of unchanged lines.
    DiffMarker,
}

/// **The mapping**: Sublime scope selectors onto the slots.
///
/// Read as an ordered list: the first entry that is a *prefix* of the scope
/// wins, so a more specific selector must come before the general one it
/// refines (`keyword.operator` before `keyword`). The
/// `no_selector_is_shadowed_by_a_more_general_one` test enforces that, because
/// getting it wrong is silent - the colour is merely wrong.
///
/// A `None` entry means **"matched, and deliberately carries no colour of its
/// own"**: the enclosing scope is asked instead. That is how
/// `punctuation.definition.comment` - which sits *on top of* the comment scope -
/// ends up the colour of the comment it opens rather than the colour of a
/// semicolon.
pub const SELECTORS: &[(&str, Option<SynSlot>)] = &[
    // -- comments ---------------------------------------------------------
    // The `//`, the `/*` and the `*/` belong to the comment, not to
    // punctuation. Falling through is what makes a comment one colour.
    ("punctuation.definition.comment", None),
    ("comment", Some(SynSlot::Comment)),
    // -- strings ----------------------------------------------------------
    ("punctuation.definition.string", None),
    // An escape and a `printf` placeholder are the two things inside a string
    // that are not string *content*, and every theme picks them out.
    ("constant.character.escape", Some(SynSlot::Constant)),
    ("constant.other.placeholder", Some(SynSlot::Constant)),
    ("string", Some(SynSlot::String)),
    // -- keywords ---------------------------------------------------------
    ("keyword.operator", Some(SynSlot::Operator)),
    ("keyword", Some(SynSlot::Keyword)),
    // `storage.type` is Sublime's scope for a *declaration keyword* - `fn`,
    // `let`, `class`, `int` - and not for a type's name. Type names arrive as
    // `entity.name.type` or `support.type`, below. Colouring `storage` as a
    // type is the classic mistake here and paints every `let` in Rust the
    // colour of `Vec`.
    ("storage", Some(SynSlot::Keyword)),
    // `self`, `this`, `super`: spelled as variables, read as keywords.
    ("variable.language", Some(SynSlot::Keyword)),
    // -- numbers and constants --------------------------------------------
    ("constant.numeric", Some(SynSlot::Number)),
    ("constant.language", Some(SynSlot::Constant)),
    ("constant", Some(SynSlot::Constant)),
    // -- named entities ---------------------------------------------------
    ("entity.name.function", Some(SynSlot::Function)),
    ("entity.name.method", Some(SynSlot::Function)),
    ("entity.name.macro", Some(SynSlot::Function)),
    ("entity.name.label", Some(SynSlot::Variable)),
    // An HTML/XML tag reads as the language's own vocabulary, like a keyword.
    ("entity.name.tag", Some(SynSlot::Keyword)),
    ("entity.name.namespace", Some(SynSlot::Type)),
    // class, struct, enum, trait, union, interface, type - all one slot.
    ("entity.name", Some(SynSlot::Type)),
    ("entity.other.attribute-name", Some(SynSlot::Variable)),
    ("entity.other.inherited-class", Some(SynSlot::Type)),
    // -- names the library provides ---------------------------------------
    ("support.function", Some(SynSlot::Function)),
    ("support.macro", Some(SynSlot::Function)),
    ("support.method", Some(SynSlot::Function)),
    ("support.type", Some(SynSlot::Type)),
    ("support.class", Some(SynSlot::Type)),
    ("support.module", Some(SynSlot::Type)),
    ("support.constant", Some(SynSlot::Constant)),
    ("support", Some(SynSlot::Constant)),
    // -- variables --------------------------------------------------------
    ("variable.function", Some(SynSlot::Function)),
    ("variable.annotation", Some(SynSlot::Variable)),
    ("variable", Some(SynSlot::Variable)),
    // -- markup (Markdown, and the README a user actually opens) ----------
    ("punctuation.definition.heading", None),
    ("punctuation.definition.list_item", None),
    ("markup.heading", Some(SynSlot::Keyword)),
    ("markup.raw", Some(SynSlot::String)),
    ("markup.underline.link", Some(SynSlot::Constant)),
    ("markup.list", Some(SynSlot::Punctuation)),
    ("markup.quote", Some(SynSlot::Comment)),
    // -- punctuation ------------------------------------------------------
    // The catch-all for the family above: anything that *opens* something
    // takes the colour of the thing it opens.
    ("punctuation.definition", None),
    ("punctuation", Some(SynSlot::Punctuation)),
    // -- the scopes that are structure, not colour ------------------------
    // Listed rather than omitted so that the fall-through is deliberate: a
    // file's ordinary body is `source.rust` / `text.html.markdown` all the way
    // down and must render in `viewer.fg`, or the screen is a wall of colour.
    // `invalid.illegal` is here because the design has no error slot and a
    // wrong colour would be a worse lie than none.
    ("meta", None),
    ("source", None),
    ("text", None),
    ("invalid", None),
];

/// [`SELECTORS`] compiled once into syntect's interned scopes.
///
/// `Scope::new` locks a process-global repository and `Scope::build_string`
/// allocates, so neither belongs on the per-line path. `Scope::is_prefix_of` is
/// two masked XORs, which does.
fn selectors() -> &'static [(Scope, Option<SynSlot>)] {
    static TABLE: OnceLock<Vec<(Scope, Option<SynSlot>)>> = OnceLock::new();
    TABLE.get_or_init(|| {
        SELECTORS
            .iter()
            .filter_map(|(name, slot)| Scope::new(name).ok().map(|scope| (scope, *slot)))
            .collect()
    })
}

impl SynSlot {
    /// The slot's name inside its theme table, as spelled in a theme file.
    ///
    /// The syntax slots are `[syn]` and the four diff slots are `[diff]`;
    /// [`SynSlot::group`] says which.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Keyword => "keyword",
            Self::String => "string",
            Self::Comment => "comment",
            Self::Type => "type",
            Self::Function => "function",
            Self::Number => "number",
            Self::Constant => "constant",
            Self::Operator => "operator",
            Self::Punctuation => "punctuation",
            Self::Variable => "variable",
            Self::DiffAdded => "added",
            Self::DiffRemoved => "removed",
            Self::DiffHeader => "header",
            Self::DiffMarker => "marker",
        }
    }

    /// The colour this slot carries in a theme.
    pub fn color(self, theme: &crate::config::Theme) -> crate::config::Rgb {
        let syn = &theme.syn;
        match self {
            Self::Keyword => syn.keyword,
            Self::String => syn.string,
            Self::Comment => syn.comment,
            Self::Type => syn.type_,
            Self::Function => syn.function,
            Self::Number => syn.number,
            Self::Constant => syn.constant,
            Self::Operator => syn.operator,
            Self::Punctuation => syn.punctuation,
            Self::Variable => syn.variable,
            Self::DiffAdded => theme.diff.added,
            Self::DiffRemoved => theme.diff.removed,
            Self::DiffHeader => theme.diff.header,
            Self::DiffMarker => theme.diff.marker,
        }
    }

    /// **The mapping**: a scope stack onto a theme slot.
    ///
    /// The stack is examined from the top down, because the innermost scope is
    /// the most specific: `source.rust meta.function.rust
    /// entity.name.function.rust` is a function name, not a chunk of source.
    /// Each scope is looked up in [`SELECTORS`]; a scope that matches a `None`
    /// entry, or matches nothing at all, hands the question to the scope
    /// outside it.
    ///
    /// `None` overall means no slot claims this run and it is drawn in
    /// `viewer.fg`, which is what makes the ordinary body of a file readable
    /// rather than a wall of colour.
    pub fn for_scope(stack: &[Scope]) -> Option<Self> {
        for scope in stack.iter().rev() {
            for (selector, slot) in selectors() {
                if selector.is_prefix_of(*scope) {
                    if slot.is_some() {
                        return *slot;
                    }
                    // A deliberate abstention: ask the enclosing scope.
                    break;
                }
            }
        }
        None
    }
}

/// `bat`'s curated syntax set, built once for the process.
///
/// Loading it is deserializing a few hundred kilobytes and is done lazily, so
/// an `hcmd` session that never opens the viewer never pays for it - and
/// the "the file opens instantly" survives, because it happens once
/// and never on a per-file path.
///
/// It is also the *only* set in the process, which matters: syntect documents
/// that a `ParseState` must be used with the set it was made from, so having
/// exactly one removes the mismatch by construction.
///
/// `extra_newlines` rather than `extra_no_newlines`: syntect's parser wants the
/// line's trailing `\n` for the syntaxes whose rules match end-of-line, and
/// [`Highlighter::line`] supplies one when the caller's line has none.
pub fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(two_face::syntax::extra_newlines)
}

/// How much of one line is worth parsing.
///
/// Past this, the tail is drawn plain. A line this long is not being read as
/// code by anybody - the viewer already cuts at
/// [`crate::viewer::text::MAX_LINE_BYTES`] - and it is the one input that could
/// make a single `parse_line` call expensive enough to be seen as a dropped
/// frame.
pub const MAX_HIGHLIGHT_LINE: usize = 16 * 1024;

/// How long one [`Highlighter`] may spend parsing before it gives up and draws
/// the rest plain.
///
/// This is a budget for a *run* of lines - in practice one screenful, since
/// resuming from a checkpoint hands out a fresh one. It exists so that a
/// hostile file degrades to plain text instead of stopping the event loop, and
/// it is deliberately generous rather than tuned: the thing it must catch is a
/// runaway, and the cost of catching an honest file by mistake is a screen of
/// plain text.
///
/// Generous, specifically, because syntect compiles a syntax's regexes **on
/// first use**, not at load: measured here, the first few lines of the first
/// Rust file in a debug build cost 7-12 ms each while the rest cost 0.1 ms. A
/// budget tight enough to be a frame target would spend itself on that warm-up
/// and stall a perfectly ordinary file.
pub const PARSE_BUDGET: Duration = Duration::from_millis(250);

/// One highlighted run within a line: a byte range and the slot it takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// Byte range within the line that was passed in.
    pub range: std::ops::Range<usize>,
    /// The theme slot, or `None` for ordinary body text.
    pub slot: Option<SynSlot>,
}

/// A resumable highlighting position.
///
/// One of these is a place in the file, not a copy of it: **cloning one is
/// taking a checkpoint**, which is how the visible window is highlighted
/// without re-parsing everything above it. The clone carries the parse
/// position and a *fresh* time budget, because the budget belongs to the run of
/// lines about to be parsed and not to the position.
#[derive(Debug)]
pub struct Highlighter {
    state: ParseState,
    stack: ScopeStack,
    budget: Duration,
    spent: Duration,
    stalled: bool,
}

impl Clone for Highlighter {
    /// Take a checkpoint. See the type's note: the budget does not travel with
    /// the position.
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            stack: self.stack.clone(),
            budget: self.budget,
            spent: Duration::ZERO,
            stalled: false,
        }
    }
}

impl Highlighter {
    /// Start highlighting a file whose name is `name` and whose first line is
    /// `first_line`.
    ///
    /// `None` when no syntax matches, which is the ordinary answer for a log
    /// file and means "draw it plain" rather than "fail".
    pub fn for_file(name: &str, first_line: &str) -> Option<Self> {
        let set = syntaxes();
        let syntax = syntax_for(set, name, first_line)?;
        Some(Self::for_syntax(syntax))
    }

    /// Start from a named syntax.
    ///
    /// The syntax is re-resolved inside [`syntaxes`] by name, so a reference
    /// that came from some other `SyntaxSet` cannot produce a `ParseState`
    /// holding another set's context indexes.
    pub fn for_syntax(syntax: &SyntaxReference) -> Self {
        let set = syntaxes();
        let syntax = set.find_syntax_by_name(&syntax.name).unwrap_or(syntax);
        Self {
            state: ParseState::new(syntax),
            stack: ScopeStack::new(),
            budget: PARSE_BUDGET,
            spent: Duration::ZERO,
            stalled: false,
        }
    }

    /// Replace the parse-time budget for the run of lines from here.
    ///
    /// `Duration::ZERO` means "one line, then plain": the budget is checked
    /// after each line, so a line already started is always finished.
    pub const fn set_budget(&mut self, budget: Duration) {
        self.budget = budget;
    }

    /// True once the budget ran out and every further line comes back plain.
    ///
    /// A stalled highlighter is not an error and not sticky: resuming from a
    /// checkpoint - that is, cloning it - starts again with a full budget.
    pub const fn is_stalled(&self) -> bool {
        self.stalled
    }

    /// Take a checkpoint of this position. Exactly [`Clone::clone`], named for
    /// what the caller is doing.
    pub fn checkpoint(&self) -> Self {
        self.clone()
    }

    /// Advance one line and report its spans.
    ///
    /// **Never fails.** `syntect` can return a parse error on a pathological
    /// syntax, and a viewer that refused to draw a line because a grammar had
    /// an opinion about it would be worse than one that drew it plain - so an
    /// error yields one unhighlighted span and highlighting carries on.
    ///
    /// The line may be passed with or without its trailing newline. Where it
    /// has none, one is supplied for the parser and kept out of the spans: the
    /// syntax set is built with newlines (see [`syntaxes`]) and a line-comment
    /// rule that ends at `\n` would otherwise never close, bleeding the comment
    /// into every line below it.
    ///
    /// The returned spans always tile the line exactly - in order, no gaps, no
    /// overlaps, covering `0..line.len()` - so the renderer can walk them
    /// without checking.
    pub fn line(&mut self, line: &str) -> Vec<Span> {
        if self.stalled {
            return vec![plain(line.len())];
        }
        // Bound 2: only a prefix of a very long line is worth parsing.
        let cut = floor_char_boundary(line, MAX_HIGHLIGHT_LINE);
        let head = line.get(..cut).unwrap_or("");

        let owned;
        let parsed: &str = if head.ends_with('\n') {
            head
        } else {
            owned = format!("{head}\n");
            &owned
        };

        let started = Instant::now();
        let ops = self.state.parse_line(parsed, syntaxes());
        self.spent = self.spent.saturating_add(started.elapsed());
        // Bound 3: checked after the line, so a line is never half-parsed.
        if self.spent >= self.budget {
            self.stalled = true;
        }

        let Ok(ops) = ops else {
            return vec![plain(line.len())];
        };

        let mut spans: Vec<Span> = Vec::new();
        let mut at = 0_usize;
        for (offset, op) in &ops {
            let offset = (*offset).min(cut);
            if offset > at {
                push_span(
                    &mut spans,
                    at..offset,
                    SynSlot::for_scope(self.stack.as_slice()),
                );
                at = offset;
            }
            // An op that the stack rejects is dropped rather than aborting the
            // line: the worst case is one run coloured as its neighbour.
            let _ = self.stack.apply(op);
        }
        if at < cut {
            push_span(
                &mut spans,
                at..cut,
                SynSlot::for_scope(self.stack.as_slice()),
            );
            at = cut;
        }
        // The unparsed tail of an over-long line, drawn plain.
        if at < line.len() {
            push_span(&mut spans, at..line.len(), None);
        }
        if spans.is_empty() {
            spans.push(plain(line.len()));
        }
        spans
    }
}

/// One span covering `0..len` in no slot at all.
const fn plain(len: usize) -> Span {
    Span {
        range: 0..len,
        slot: None,
    }
}

/// The largest byte index `<= max` that starts a character.
///
/// `str::floor_char_boundary` is still unstable, and slicing a `&str` on a
/// continuation byte panics - which the design's "a byte range can split a UTF-8
/// sequence" rules out.
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if s.len() <= max {
        return s.len();
    }
    let mut at = max;
    while at > 0 && !s.is_char_boundary(at) {
        at = at.saturating_sub(1);
    }
    at
}

/// Merge a run into the tail of `spans` when it carries the same slot, so a
/// line comes back as a handful of runs rather than one per parser op.
fn push_span(spans: &mut Vec<Span>, range: std::ops::Range<usize>, slot: Option<SynSlot>) {
    if let Some(last) = spans.last_mut()
        && last.slot == slot
        && last.range.end == range.start
    {
        last.range.end = range.end;
        return;
    }
    spans.push(Span { range, slot });
}

/// Pick a syntax by file name, then by extension, then by first line
/// (the "one dependency covers every language").
///
/// The order is the one the design implies and the design needs: the name first
/// (`Dockerfile`, `Makefile`, `.bashrc` are names, not extensions), then every
/// dotted suffix longest-first so `page.blade.php` finds Blade before PHP, then
/// the first line for a shebang or a mode line. **An unknown type gets no
/// syntax rather than a guessed one**, and so does a file the set resolves to
/// `Plain Text`: both render as plain text, which is the honest answer and also
/// the free one, since a plain-text `ParseState` would be driven over every
/// visible line to produce no scopes at all.
///
/// `find_syntax_for_file` is deliberately not used: it reads the file from
/// disk, which the viewer must never do (the design - the bytes it has are
/// the window it already read).
pub fn syntax_for<'a>(
    set: &'a SyntaxSet,
    name: &str,
    first_line: &str,
) -> Option<&'a SyntaxReference> {
    let file = std::path::Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(name);

    if let Some(found) = set.find_syntax_by_extension(file) {
        return not_plain_text(found);
    }
    // `x.blade.php` -> `blade.php`, then `php`. `.bashrc` -> `bashrc`.
    let mut rest = file;
    while let Some((_, tail)) = rest.split_once('.') {
        if !tail.is_empty()
            && let Some(found) = set.find_syntax_by_extension(tail)
        {
            return not_plain_text(found);
        }
        rest = tail;
    }
    if !first_line.is_empty()
        && let Some(found) = set.find_syntax_by_first_line(first_line)
    {
        return not_plain_text(found);
    }
    None
}

/// `Plain Text` is syntect's name for "no rules", and driving a parser for it
/// costs the same as driving one that would colour something.
fn not_plain_text(syntax: &SyntaxReference) -> Option<&SyntaxReference> {
    (syntax.name != "Plain Text").then_some(syntax)
}

/// How far apart saved parse states start out, in file bytes.
///
/// One per `MAX_WINDOW`-sized read, so an ordinary scroll through a file leaves
/// a trail behind it and jumping back lands on one.
pub const CHECKPOINT_SPACING: u64 = 256 * 1024;

/// The most parse states kept, **for any file size**.
///
/// the memory rule, in the same shape as
/// [`crate::viewer::index::MAX_CHECKPOINTS`]: at the cap the store decimates -
/// drop every second state, double the spacing - so a 40 GB file and a 4 KB
/// file cost the same at rest.
pub const MAX_CHECKPOINTS: usize = 64;

/// A sparse, capped store of resumable parse states by byte offset.
///
/// "highlighting can start from a checkpoint and cover just the
/// visible window". The dense case - scrolling a line at a time - is served by
/// the previous layout's own states. This is the sparse case: a `PgDn` storm, a
/// percentage seek, `Ctrl+G`, or coming back to where you were. Without it, any
/// jump re-starts the parser at the top of the window and guesses wrong about a
/// block comment that opened off-screen; with it, a jump within
/// `max_catch_up` of a checkpoint resumes correctly for the price of a bounded
/// re-parse.
///
/// The store is *advisory* in both directions: it may return nothing, and what
/// it returns is a parse position that the caller must walk forward to the row
/// it actually wants. Nothing here reads the file - bytes come from
/// `Source::read_window` and nowhere else (contract I1).
#[derive(Debug, Clone)]
pub struct Checkpoints {
    spacing: u64,
    marks: Vec<(u64, Highlighter)>,
}

impl Default for Checkpoints {
    fn default() -> Self {
        Self::new()
    }
}

impl Checkpoints {
    /// An empty store at [`CHECKPOINT_SPACING`].
    pub const fn new() -> Self {
        Self {
            spacing: CHECKPOINT_SPACING,
            marks: Vec::new(),
        }
    }

    /// An empty store at a chosen spacing. Zero is read as one.
    pub const fn with_spacing(spacing: u64) -> Self {
        Self {
            spacing: if spacing == 0 { 1 } else { spacing },
            marks: Vec::new(),
        }
    }

    /// The current spacing, which doubles on every decimation.
    pub const fn spacing(&self) -> u64 {
        self.spacing
    }

    /// How many states are held.
    pub fn len(&self) -> usize {
        self.marks.len()
    }

    /// True while nothing has been saved.
    pub fn is_empty(&self) -> bool {
        self.marks.is_empty()
    }

    /// Throw everything away.
    ///
    /// Every saved state belongs to one syntax, one encoding and one decoding
    /// of the file, so the caller must call this when any of the three changes -
    /// `F8` re-decoding the file being the case that actually happens.
    pub fn clear(&mut self) {
        self.marks.clear();
        self.spacing = CHECKPOINT_SPACING;
    }

    /// The offsets held, lowest first.
    pub fn offsets(&self) -> Vec<u64> {
        self.marks.iter().map(|(at, _)| *at).collect()
    }

    /// Offer a state for `offset`. Kept only if it is at least [`spacing`] from
    /// the neighbours already held, so calling this for every visible row is
    /// correct and cheap.
    ///
    /// [`spacing`]: Self::spacing
    pub fn save(&mut self, offset: u64, state: &Highlighter) {
        let Err(pos) = self.marks.binary_search_by_key(&offset, |(at, _)| *at) else {
            return; // already have this exact position
        };
        if let Some(i) = pos.checked_sub(1)
            && let Some((before, _)) = self.marks.get(i)
            && offset.saturating_sub(*before) < self.spacing
        {
            return;
        }
        if let Some((after, _)) = self.marks.get(pos)
            && after.saturating_sub(offset) < self.spacing
        {
            return;
        }
        self.marks.insert(pos, (offset, state.clone()));
        if self.marks.len() > MAX_CHECKPOINTS {
            self.decimate();
        }
    }

    /// The state saved for exactly this offset, if there is one.
    pub fn exact(&self, offset: u64) -> Option<Highlighter> {
        let i = self
            .marks
            .binary_search_by_key(&offset, |(at, _)| *at)
            .ok()?;
        self.marks.get(i).map(|(_, h)| h.clone())
    }

    /// The nearest state at or before `offset`, and where it is - provided the
    /// caller would have to re-parse no more than `max_catch_up` bytes to reach
    /// `offset` from it.
    ///
    /// The bound is the caller's, not the store's, because only the caller
    /// knows what it can afford this frame (contract I9: every navigation step
    /// is bounded).
    pub fn resume(&self, offset: u64, max_catch_up: u64) -> Option<(u64, Highlighter)> {
        let pos = match self.marks.binary_search_by_key(&offset, |(at, _)| *at) {
            Ok(i) => i,
            Err(0) => return None,
            Err(i) => i.saturating_sub(1),
        };
        let (at, state) = self.marks.get(pos)?;
        (offset.saturating_sub(*at) <= max_catch_up).then(|| (*at, state.clone()))
    }

    /// Halve the store and double the spacing, so memory is a constant.
    fn decimate(&mut self) {
        let mut i = 0_usize;
        self.marks.retain(|_| {
            let keep = i.is_multiple_of(2);
            i = i.saturating_add(1);
            keep
        });
        self.spacing = self.spacing.saturating_mul(2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slot the line assigns to the first byte of `needle`.
    fn slot_at(spans: &[Span], line: &str, needle: &str) -> Option<SynSlot> {
        let at = line.find(needle)?;
        spans
            .iter()
            .find(|s| s.range.contains(&at))
            .and_then(|s| s.slot)
    }

    fn scope_slot(names: &[&str]) -> Option<SynSlot> {
        let stack: Vec<Scope> = names
            .iter()
            .filter_map(|n| Scope::new(n).ok())
            .collect::<Vec<_>>();
        assert_eq!(stack.len(), names.len(), "a test scope failed to parse");
        SynSlot::for_scope(&stack)
    }

    #[test]
    fn the_curated_set_covers_the_gaps_the_spec_names() {
        let set = syntaxes();
        // "fills syntect's default gaps (TOML, Rust, Dockerfile,
        // Nix, …)".
        for name in [
            "a.rs",
            "a.toml",
            "Dockerfile",
            "a.nix",
            "a.md",
            "a.py",
            "a.xml",
        ] {
            assert!(syntax_for(set, name, "").is_some(), "no syntax for {name}");
        }
    }

    #[test]
    fn a_file_with_no_syntax_highlights_as_nothing_rather_than_failing() {
        assert!(Highlighter::for_file("mystery.qqq", "just some words").is_none());
    }

    #[test]
    fn plain_text_is_no_syntax_rather_than_a_parser_that_finds_nothing() {
        // `.txt` resolves to syntect's `Plain Text`, which has no rules. Driving
        // a ParseState over it would cost a parse per visible line to produce
        // no scopes at all.
        assert!(Highlighter::for_file("notes.txt", "hello").is_none());
    }

    #[test]
    fn a_shebang_picks_the_syntax_when_the_name_cannot() {
        assert!(Highlighter::for_file("run", "#!/bin/sh\n").is_some());
    }

    #[test]
    fn a_path_is_reduced_to_its_file_name_and_dotted_suffixes_are_tried() {
        let set = syntaxes();
        assert!(syntax_for(set, "/home/x/src/main.rs", "").is_some());
        // The whole name first, then each suffix: a compound extension finds
        // the inner language when the outer one is unknown.
        let php = syntax_for(set, "page.qqq.php", "").map(|s| s.name.clone());
        assert_eq!(
            php.as_deref(),
            syntax_for(set, "a.php", "").map(|s| s.name.as_str())
        );
        // A dotfile is a name with a leading dot, not an extension.
        assert!(syntax_for(set, ".bashrc", "").is_some());
    }

    // ---------------------------------------------------------- mapping ----

    #[test]
    fn no_selector_is_shadowed_by_a_more_general_one() {
        // The table is scanned in order and the first prefix match wins, so a
        // general selector placed before the specific one it refines would
        // silently swallow it. This is the check that keeps the ordering
        // honest as arms are added.
        let scopes: Vec<Scope> = SELECTORS
            .iter()
            .filter_map(|(name, _)| Scope::new(name).ok())
            .collect();
        assert_eq!(scopes.len(), SELECTORS.len(), "a selector failed to parse");
        for (i, a) in scopes.iter().enumerate() {
            for (j, b) in scopes.iter().enumerate().skip(i.saturating_add(1)) {
                assert!(
                    !(a.is_prefix_of(*b) && a != b),
                    "`{}` shadows `{}`; the specific one must come first",
                    SELECTORS[i].0,
                    SELECTORS[j].0,
                );
            }
        }
    }

    #[test]
    fn storage_is_a_declaration_keyword_and_not_a_type() {
        // The trap named in colouring `storage.type`
        // as a type paints every `let` the colour of `Vec`.
        assert_eq!(
            scope_slot(&["source.rust", "storage.type.rust"]),
            Some(SynSlot::Keyword)
        );
        assert_eq!(
            scope_slot(&["source.rust", "entity.name.type.rust"]),
            Some(SynSlot::Type)
        );
    }

    #[test]
    fn a_comments_own_punctuation_is_the_comments_colour() {
        // The other trap: `punctuation.definition.comment` sits on top of the
        // comment scope, so the innermost match would paint `//` differently
        // from the comment it opens.
        assert_eq!(
            scope_slot(&[
                "source.rust",
                "comment.line.double-slash.rust",
                "punctuation.definition.comment.rust",
            ]),
            Some(SynSlot::Comment)
        );
        assert_eq!(
            scope_slot(&[
                "source.rust",
                "string.quoted.double.rust",
                "punctuation.definition.string.begin.rust",
            ]),
            Some(SynSlot::String)
        );
    }

    #[test]
    fn the_mapping_covers_the_families_a_real_file_produces() {
        for (scopes, want) in [
            (vec!["source.c", "keyword.control.c"], SynSlot::Keyword),
            (
                vec!["source.c", "keyword.operator.arithmetic.c"],
                SynSlot::Operator,
            ),
            (
                vec!["source.c", "constant.numeric.integer.c"],
                SynSlot::Number,
            ),
            (vec!["source.c", "constant.language.c"], SynSlot::Constant),
            (
                vec![
                    "source.rust",
                    "string.quoted.double.rust",
                    "constant.character.escape.rust",
                ],
                SynSlot::Constant,
            ),
            (
                vec!["source.py", "entity.name.function.py"],
                SynSlot::Function,
            ),
            (
                vec!["source.py", "support.function.builtin.py"],
                SynSlot::Function,
            ),
            (vec!["source.py", "support.type.py"], SynSlot::Type),
            (vec!["source.js", "variable.function.js"], SynSlot::Function),
            (
                vec!["source.js", "variable.other.readwrite.js"],
                SynSlot::Variable,
            ),
            (
                vec!["source.js", "variable.language.this.js"],
                SynSlot::Keyword,
            ),
            (vec!["text.html", "entity.name.tag.html"], SynSlot::Keyword),
            (
                vec!["text.html", "entity.other.attribute-name.html"],
                SynSlot::Variable,
            ),
            (
                vec!["source.c", "punctuation.separator.c"],
                SynSlot::Punctuation,
            ),
            (
                vec!["text.html.markdown", "markup.heading.1.markdown"],
                SynSlot::Keyword,
            ),
            (
                vec!["text.html.markdown", "markup.raw.inline.markdown"],
                SynSlot::String,
            ),
        ] {
            assert_eq!(scope_slot(&scopes), Some(want), "{scopes:?}");
        }
    }

    #[test]
    fn structure_scopes_carry_no_colour_of_their_own() {
        // A file's ordinary body must render in `viewer.fg`.
        assert_eq!(scope_slot(&["source.rust"]), None);
        assert_eq!(scope_slot(&["source.rust", "meta.function.rust"]), None);
        assert_eq!(scope_slot(&["text.html.markdown"]), None);
        // the design has no slot for an error, and a wrong colour is a worse
        // lie than no colour.
        assert_eq!(scope_slot(&["source.rust", "invalid.illegal.rust"]), None);
        assert_eq!(SynSlot::for_scope(&[]), None);
    }

    // ------------------------------------------------------------ lines ----

    #[test]
    fn rust_source_maps_onto_the_syn_slots() {
        let mut h = Highlighter::for_file("x.rs", "").expect("rust syntax");
        let line = "let n = 1; // hi\n";
        let spans = h.line(line);
        assert_eq!(slot_at(&spans, line, "let"), Some(SynSlot::Keyword));
        assert_eq!(slot_at(&spans, line, "1"), Some(SynSlot::Number));
        assert_eq!(slot_at(&spans, line, "// hi"), Some(SynSlot::Comment));
    }

    #[test]
    fn a_string_is_a_string_and_the_body_is_plain() {
        let mut h = Highlighter::for_file("x.rs", "").expect("rust syntax");
        let line = "let s = \"hello\";\n";
        let spans = h.line(line);
        assert_eq!(slot_at(&spans, line, "hello"), Some(SynSlot::String));
    }

    #[test]
    fn spans_cover_the_line_exactly_once_and_in_order() {
        let mut h = Highlighter::for_file("x.rs", "").expect("rust syntax");
        for line in [
            "fn main() {\n",
            "    let v: Vec<u8> = vec![1, 2, 3]; // note\n",
            "}\n",
            "\n",
            "",
            "no newline here",
        ] {
            let spans = h.line(line);
            let mut at = 0;
            for s in &spans {
                assert_eq!(s.range.start, at, "{line:?} -> {spans:?}");
                assert!(s.range.end >= s.range.start, "{line:?} -> {spans:?}");
                at = s.range.end;
            }
            assert_eq!(at, line.len(), "{line:?} -> {spans:?}");
        }
    }

    #[test]
    fn a_line_comment_does_not_bleed_when_the_terminator_was_trimmed() {
        // The viewer hands over the decoded line *without* its line break -
        // `LineTerm::trim_break` takes it - while the syntax set is built with
        // newlines. A `//` rule that ends at `\n` would then never close and
        // every line below would be a comment.
        let mut h = Highlighter::for_file("x.rs", "").expect("rust syntax");
        let first = "// a comment";
        assert_eq!(slot_at(&h.line(first), first, "//"), Some(SynSlot::Comment));
        let second = "let n = 1;";
        let spans = h.line(second);
        assert_eq!(slot_at(&spans, second, "let"), Some(SynSlot::Keyword));
        assert_eq!(slot_at(&spans, second, "1"), Some(SynSlot::Number));
    }

    #[test]
    fn a_block_comment_still_spans_lines_without_terminators() {
        // The other half of the same story: supplying the newline must not
        // *close* anything the file left open.
        let mut h = Highlighter::for_file("x.rs", "").expect("rust syntax");
        h.line("/* opened");
        let line = "still inside";
        assert_eq!(
            slot_at(&h.line(line), line, "still"),
            Some(SynSlot::Comment)
        );
    }

    #[test]
    fn an_over_long_line_is_parsed_only_as_far_as_it_is_worth() {
        let mut h = Highlighter::for_file("x.rs", "").expect("rust syntax");
        let line = format!("let s = \"{}\";\n", "x".repeat(MAX_HIGHLIGHT_LINE * 2));
        let spans = h.line(&line);
        // The bound is structural, and that is what is asserted. This used to
        // be a stopwatch - "elapsed < 2 seconds" - which measures the machine
        // rather than the code and failed on a loaded CI runner while the cap
        // was working perfectly. What the cap actually promises is that
        // nothing past MAX_HIGHLIGHT_LINE was handed to the parser, so
        // everything out there comes back as ordinary text.
        let cut = MAX_HIGHLIGHT_LINE;
        assert!(
            spans.iter().any(|s| s.slot.is_some()),
            "the head is still highlighted: {spans:?}"
        );
        for s in &spans {
            assert!(
                s.range.start >= cut || s.slot.is_some() || s.range.end <= cut,
                "a span straddles the cut: {s:?}"
            );
            assert!(
                s.range.start < cut || s.slot.is_none(),
                "nothing past the cut was parsed: {s:?}"
            );
        }
        let mut at = 0;
        for s in &spans {
            assert_eq!(s.range.start, at);
            at = s.range.end;
        }
        assert_eq!(at, line.len(), "the tail is still covered");
        assert_eq!(
            spans.last().and_then(|s| s.slot),
            None,
            "the unparsed tail is drawn plain"
        );
        assert_eq!(slot_at(&spans, &line, "let"), Some(SynSlot::Keyword));
    }

    #[test]
    fn a_multibyte_character_is_never_split_by_the_line_cap() {
        // A cut that landed inside a UTF-8 sequence would panic on the slice.
        let filler = "é".repeat(MAX_HIGHLIGHT_LINE);
        let line = format!("// {filler}");
        assert!(!line.is_char_boundary(MAX_HIGHLIGHT_LINE));
        let mut h = Highlighter::for_file("x.rs", "").expect("rust syntax");
        let spans = h.line(&line);
        assert_eq!(spans.last().map(|s| s.range.end), Some(line.len()));
    }

    // ------------------------------------------------------ the budget ----

    #[test]
    fn a_spent_budget_stalls_into_plain_text_instead_of_stopping_the_frame() {
        let mut h = Highlighter::for_file("x.rs", "").expect("rust syntax");
        h.set_budget(Duration::ZERO);
        assert!(!h.is_stalled());
        // The line already started is always finished: the budget is checked
        // after it.
        let first = "let n = 1;";
        assert_eq!(
            slot_at(&h.line(first), first, "let"),
            Some(SynSlot::Keyword)
        );
        assert!(h.is_stalled());
        let second = "let m = 2;";
        let spans = h.line(second);
        assert_eq!(spans, vec![plain(second.len())]);
    }

    #[test]
    fn resuming_from_a_checkpoint_hands_out_a_fresh_budget() {
        let mut h = Highlighter::for_file("x.rs", "").expect("rust syntax");
        let saved = h.checkpoint();
        h.set_budget(Duration::ZERO);
        h.line("let n = 1;");
        assert!(h.is_stalled());
        let mut resumed = saved.clone();
        assert!(
            !resumed.is_stalled(),
            "a checkpoint is a position, not a budget"
        );
        let line = "let n = 1;";
        assert_eq!(
            slot_at(&resumed.line(line), line, "let"),
            Some(SynSlot::Keyword)
        );
        assert!(!resumed.is_stalled());
    }

    // ------------------------------------------------------ checkpoints ----

    #[test]
    fn a_saved_state_resumes_where_it_left_off() {
        // the whole reason for choosing syntect: a `ParseState` is
        // a resumable position, so the visible window can be highlighted from a
        // checkpoint instead of from the top of the file.
        let mut h = Highlighter::for_file("x.rs", "").expect("rust syntax");
        h.line("/* a block comment starts\n");
        let saved = h.checkpoint();

        let mid = h.line("still inside the comment\n");
        assert_eq!(mid.first().and_then(|s| s.slot), Some(SynSlot::Comment));

        let mut resumed = saved;
        let again = resumed.line("still inside the comment\n");
        assert_eq!(again, mid, "resuming reproduces the same spans");
    }

    #[test]
    fn checkpoints_are_sparse_so_saving_every_row_is_free() {
        let h = Highlighter::for_file("x.rs", "").expect("rust syntax");
        let mut marks = Checkpoints::with_spacing(1000);
        for at in (0..5000).step_by(10) {
            marks.save(at, &h);
        }
        assert_eq!(marks.offsets(), vec![0, 1000, 2000, 3000, 4000]);
    }

    #[test]
    fn a_checkpoint_resumes_a_jump_within_the_callers_budget() {
        let mut h = Highlighter::for_file("x.rs", "").expect("rust syntax");
        h.line("/* opened far above");
        let mut marks = Checkpoints::with_spacing(100);
        marks.save(1_000, &h);

        assert!(marks.exact(1_000).is_some());
        assert!(marks.exact(1_001).is_none());
        assert!(
            marks.resume(500, u64::MAX).is_none(),
            "never resume forwards"
        );

        let (at, mut resumed) = marks.resume(1_050, 100).expect("within the budget");
        assert_eq!(at, 1_000);
        let line = "still inside";
        assert_eq!(
            slot_at(&resumed.line(line), line, "still"),
            Some(SynSlot::Comment)
        );

        assert!(
            marks.resume(1_050, 8).is_none(),
            "too far to catch up on this frame"
        );
    }

    #[test]
    fn the_store_decimates_so_memory_is_a_constant_for_any_file_size() {
        // memory is a function of the window, not of the file.
        let h = Highlighter::for_file("x.rs", "").expect("rust syntax");
        let mut marks = Checkpoints::with_spacing(1);
        for at in 0..10_000 {
            marks.save(at, &h);
        }
        assert!(marks.len() <= MAX_CHECKPOINTS, "{} kept", marks.len());
        assert!(marks.spacing() > 1, "the spacing doubles as it decimates");
        // The oldest anchor survives every decimation, so the top of the file
        // is always resumable.
        assert_eq!(marks.offsets().first().copied(), Some(0));
        assert!(marks.resume(9_999, u64::MAX).is_some());

        marks.clear();
        assert!(marks.is_empty());
        assert_eq!(marks.spacing(), CHECKPOINT_SPACING);
    }

    #[test]
    fn every_slot_names_a_real_theme_key() {
        let theme = crate::config::Theme::blue();
        for slot in [
            SynSlot::Keyword,
            SynSlot::String,
            SynSlot::Comment,
            SynSlot::Type,
            SynSlot::Function,
            SynSlot::Number,
            SynSlot::Constant,
            SynSlot::Operator,
            SynSlot::Punctuation,
            SynSlot::Variable,
        ] {
            assert_eq!(
                theme.syn.slot(slot.id()),
                Some(slot.color(&theme)),
                "syn.{} is not the slot it says it is",
                slot.id()
            );
        }
    }
}
