//! `keymap.toml`.
//!
//! Shape: `[global]` / `[panel]` / `[cmdline]` / `[viewer]` / `[dialog]` /
//! `[console]` tables of `action_id = ["binding", "fallback"]`. Every list entry
//! is a binding for the same action; the second is usually the legacy-terminal
//! fallback.
//!
//! # Resolution order
//!
//! 1. Context-specific binding from `keymap.toml`.
//! 2. Global binding from `keymap.toml`.
//! 3. Built-in default for the context.
//! 4. Fall through to the context's default text handling - quick search for a
//!    panel, insert for the command line.
//!
//! The four steps are four real tables, consulted in that order: the user's
//! context table, the user's `[global]` table, the built-in context table, and
//! the built-in `[global]` table. [`Keymap::builtin`] fills the built-in layer
//! with the full compiled-in Total Commander layout; a user file goes into the
//! layer above it. Step 4 is what [`Resolution::Unbound`] tells the caller.
//!
//! Keeping the two layers apart is what makes step 2 outrank step 3, and that
//! is not a detail: a hand-written `[global] hotlist = ["backspace"]` has to win
//! over the built-in `[panel] parent = ["backspace"]`, or the user's own file is
//! silently dead. Collapsing the layers gives the reverse of the specified
//! order.
//!
//! **Layering rule.** When a user file mentions an action, that action's
//! built-in bindings *in the same table* are removed first, then the user's list
//! is installed. So `quit = ["ctrl+q"]` really means "quit is `Ctrl+Q` and
//! nothing else", which is what someone hand-editing a keymap expects. Actions
//! the file does not mention keep their defaults, so a partial file is useful.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyModifiers};

use crate::input::{Action, Binding, KeyPress};

/// Which keymap table a key event is resolved against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KeyContext {
    /// A panel has focus.
    Panel,
    /// The command line has focus.
    CmdLine,
    /// The viewer has focus.
    Viewer,
    /// A modal dialog has focus.
    Dialog,
    /// The console PTY has focus.
    Console,
}

impl KeyContext {
    /// Every context, in `keymap.toml` order.
    pub const ALL: &'static [Self] = &[
        Self::Panel,
        Self::CmdLine,
        Self::Viewer,
        Self::Dialog,
        Self::Console,
    ];

    /// The table name in `keymap.toml`.
    pub const fn id(&self) -> &'static str {
        match self {
            Self::Panel => "panel",
            Self::CmdLine => "cmdline",
            Self::Viewer => "viewer",
            Self::Dialog => "dialog",
            Self::Console => "console",
        }
    }

    /// Parse a table name.
    pub fn from_id(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.id() == s)
    }
}

/// What resolving a key press produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// The key is bound.
    Action(Action),
    /// The key is the first half of a chord. The caller remembers it and
    /// resolves the next press with [`Keymap::resolve_chord`].
    ChordPending,
    /// Nothing is bound. The caller falls through to the context's default text
    /// handling.
    Unbound,
}

/// What [`Keymap::describe`] prints for an action nothing is bound to.
///
/// A row that said nothing at all would read as a rendering bug; the design's
/// page lists every action, so the unbound ones have to say what they are.
pub const UNBOUND: &str = "(unbound)";

/// How [`Keymap::describe`] marks a binding this terminal cannot deliver.
///
pub const UNAVAILABLE: &str = "(unavailable)";

/// What [`Keymap::describe`] adds when **every** binding of an action is
/// undeliverable on this terminal (item 2 asks for a fallback for
/// every affected key, so this text appearing on the `F1` page is a keymap
/// bug rather than a terminal one, and it says so where it will be seen).
pub const NO_FALLBACK: &str = " (no fallback binding)";

/// Does this binding need the enhanced keyboard protocol, or collide with a key
/// that outranks it, on a legacy terminal?
///
/// Two rules rather than one, and the design keeps them apart deliberately:
/// [`KeyPress::needs_enhanced_protocol`] is the "the terminal cannot send it",
/// and `Ctrl+H` is the "it arrives and `Backspace` wins".
/// `crate::input::resolve_ctrl_h` is the code that makes the second true, and
/// the design asks for exactly this page to say so: "The help screen marks
/// `Ctrl+H` as unavailable and shows `Alt+.` in its place when running on a
/// legacy terminal, so this never has to be debugged."
///
/// A chord counts as undeliverable when **either** half is, because a chord
/// that cannot be completed is not a binding.
fn binding_is_undeliverable(binding: Binding) -> bool {
    let ctrl_h = KeyPress::new(KeyCode::Char('h'), KeyModifiers::CONTROL);
    let press_is = |press: KeyPress| press.needs_enhanced_protocol() || press == ctrl_h;
    match binding {
        Binding::Key(key) => press_is(key),
        Binding::Chord(first, second) => press_is(first) || press_is(second),
    }
}

/// One binding, written the way the design writes keys.
///
/// A chord is its two presses with a space between them, which is exactly how
/// `keymap.toml` spells one - so a reader can copy a row of the
/// `F1` page into the file with only the capitalisation to undo.
fn pretty_binding(binding: Binding) -> String {
    match binding {
        Binding::Key(key) => pretty_key(key),
        Binding::Chord(first, second) => format!("{} {}", pretty_key(first), pretty_key(second)),
    }
}

/// One key press, capitalised for reading rather than for parsing.
///
/// The inverse of [`KeyPress`]'s `Display`, which spells the `keymap.toml`
/// form: `ctrl+shift+3` is what a user types into the file and `Ctrl+Shift+3`
/// is what the table calls it. A character key prints **the
/// character**, so `alt+period` reads as `Alt+.` - which is the spelling
/// the design uses for `Ctrl+H`'s fallback.
fn pretty_key(key: KeyPress) -> String {
    let mut out = String::new();
    if key.mods.contains(KeyModifiers::CONTROL) {
        out.push_str("Ctrl+");
    }
    if key.mods.contains(KeyModifiers::ALT) {
        out.push_str("Alt+");
    }
    if key.mods.contains(KeyModifiers::SHIFT) {
        out.push_str("Shift+");
    }
    match key.code {
        KeyCode::Char(' ') => out.push_str("Space"),
        KeyCode::Char(c) => out.extend(c.to_uppercase()),
        KeyCode::F(n) => out.push_str(&format!("F{n}")),
        KeyCode::Enter => out.push_str("Enter"),
        KeyCode::Esc => out.push_str("Esc"),
        KeyCode::Tab => out.push_str("Tab"),
        KeyCode::BackTab => out.push_str("Shift+Tab"),
        KeyCode::Backspace => out.push_str("Backspace"),
        KeyCode::Delete => out.push_str("Delete"),
        KeyCode::Insert => out.push_str("Insert"),
        KeyCode::Home => out.push_str("Home"),
        KeyCode::End => out.push_str("End"),
        KeyCode::PageUp => out.push_str("PgUp"),
        KeyCode::PageDown => out.push_str("PgDn"),
        KeyCode::Up => out.push_str("Up"),
        KeyCode::Down => out.push_str("Down"),
        KeyCode::Left => out.push_str("Left"),
        KeyCode::Right => out.push_str("Right"),
        // Everything crossterm can report that this program never binds:
        // media keys, modifier presses, keypad codes. Printed rather than
        // dropped, so a `[terminal.sequences]` binding
        // is still legible on the page.
        other => out.push_str(&format!("{other:?}")),
    }
    out
}

/// One context's bindings.
#[derive(Debug, Clone, Default)]
struct ContextMap {
    keys: HashMap<KeyPress, Action>,
    chords: HashMap<KeyPress, HashMap<KeyPress, Action>>,
}

impl ContextMap {
    /// Bind, returning the *different* action this displaced, if any.
    ///
    /// The return value is what lets [`Keymap::overlay`] warn about two actions
    /// in one table claiming the same key. Without it the winner is decided by
    /// the order `toml` happens to iterate a table in - alphabetical, since it
    /// is a `BTreeMap` - which is not something a user can be expected to
    /// reason about, and is how `leave_virtual` came to silently swallow `Esc`.
    fn insert(&mut self, binding: Binding, action: Action) -> Option<Action> {
        let displaced = match binding {
            Binding::Key(k) => self.keys.insert(k, action),
            Binding::Chord(a, b) => self.chords.entry(a).or_default().insert(b, action),
        };
        displaced.filter(|previous| *previous != action)
    }

    /// Whether this table already binds a key.
    fn claims(&self, binding: Binding) -> bool {
        match binding {
            Binding::Key(k) => self.keys.contains_key(&k),
            Binding::Chord(a, b) => self.chords.get(&a).is_some_and(|m| m.contains_key(&b)),
        }
    }

    /// Remove every binding of `action`, so a user file replaces rather than
    /// adds. Returns nothing; a no-op when the action was not bound.
    fn remove_action(&mut self, action: Action) {
        self.keys.retain(|_, a| *a != action);
        for map in self.chords.values_mut() {
            map.retain(|_, a| *a != action);
        }
        self.chords.retain(|_, m| !m.is_empty());
    }

    fn resolve(&self, key: KeyPress) -> Resolution {
        if let Some(action) = self.keys.get(&key) {
            return Resolution::Action(*action);
        }
        if self.chords.contains_key(&key) {
            return Resolution::ChordPending;
        }
        Resolution::Unbound
    }

    fn resolve_chord(&self, first: KeyPress, second: KeyPress) -> Option<Action> {
        self.chords.get(&first)?.get(&second).copied()
    }

    fn bindings_for(&self, action: Action, out: &mut Vec<Binding>) {
        for (k, a) in &self.keys {
            if *a == action {
                out.push(Binding::Key(*k));
            }
        }
        for (first, map) in &self.chords {
            for (second, a) in map {
                if *a == action {
                    out.push(Binding::Chord(*first, *second));
                }
            }
        }
    }
}

/// One whole set of tables: a global table plus one per context.
///
/// There are two of these in a [`Keymap`] - the compiled-in defaults and the
/// user's `keymap.toml` - because the design ranks a *user* global binding
/// above a *built-in* context binding, which a single set of tables cannot
/// express.
#[derive(Debug, Clone, Default)]
struct Layer {
    global: ContextMap,
    panel: ContextMap,
    cmdline: ContextMap,
    viewer: ContextMap,
    dialog: ContextMap,
    console: ContextMap,
}

impl Layer {
    fn ctx(&self, ctx: KeyContext) -> &ContextMap {
        match ctx {
            KeyContext::Panel => &self.panel,
            KeyContext::CmdLine => &self.cmdline,
            KeyContext::Viewer => &self.viewer,
            KeyContext::Dialog => &self.dialog,
            KeyContext::Console => &self.console,
        }
    }

    fn ctx_mut(&mut self, ctx: KeyContext) -> &mut ContextMap {
        match ctx {
            KeyContext::Panel => &mut self.panel,
            KeyContext::CmdLine => &mut self.cmdline,
            KeyContext::Viewer => &mut self.viewer,
            KeyContext::Dialog => &mut self.dialog,
            KeyContext::Console => &mut self.console,
        }
    }

    /// One table: `None` is `[global]`.
    fn table_mut(&mut self, ctx: Option<KeyContext>) -> &mut ContextMap {
        match ctx {
            Some(c) => self.ctx_mut(c),
            None => &mut self.global,
        }
    }

    fn bindings_for(&self, action: Action, out: &mut Vec<Binding>) {
        self.global.bindings_for(action, out);
        for c in KeyContext::ALL {
            self.ctx(*c).bindings_for(action, out);
        }
    }
}

/// Which of the two layers a binding goes into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayerId {
    /// The compiled-in Total Commander layout.
    Builtin,
    /// The user's `keymap.toml` (the design steps 1 and 2).
    User,
}

/// The whole keymap: the user's `keymap.toml` over the compiled-in defaults,
/// each a global table plus one table per context.
#[derive(Debug, Clone, Default)]
pub struct Keymap {
    builtin: Layer,
    user: Layer,
    /// Warnings collected while loading. Never fatal.
    pub warnings: Vec<String>,
}

impl Keymap {
    fn layer_mut(&mut self, layer: LayerId) -> &mut Layer {
        match layer {
            LayerId::Builtin => &mut self.builtin,
            LayerId::User => &mut self.user,
        }
    }

    /// Bind one action in one context, in the user layer, so it outranks the
    /// compiled-in defaults exactly the way a line in `keymap.toml` does.
    ///
    /// Public so a later milestone can build a keymap programmatically; loading
    /// goes through [`Keymap::load`].
    pub fn bind(&mut self, ctx: Option<KeyContext>, binding: Binding, action: Action) {
        self.bind_reporting(LayerId::User, ctx, binding, action);
    }

    /// [`Keymap::bind`], in a named layer, reporting the different action it
    /// displaced.
    fn bind_reporting(
        &mut self,
        layer: LayerId,
        ctx: Option<KeyContext>,
        binding: Binding,
        action: Action,
    ) -> Option<Action> {
        self.layer_mut(layer).table_mut(ctx).insert(binding, action)
    }

    /// The four tables a key is resolved against, in the design order:
    /// user context, user global, built-in context, built-in global.
    fn tables(&self, ctx: KeyContext) -> [&ContextMap; 4] {
        [
            self.user.ctx(ctx),
            &self.user.global,
            self.builtin.ctx(ctx),
            &self.builtin.global,
        ]
    }

    /// Resolve a key press, following the design steps 1-3.
    ///
    /// The user's `[global]` table is consulted **before** the built-in
    /// defaults for the context, which is the ordering the spec gives and the
    /// reason the two layers are kept apart: `[global] hotlist = ["backspace"]`
    /// in a hand-written keymap has to beat the built-in `[panel] parent`.
    ///
    /// `key` is normalised for you; pass the raw [`KeyPress`] from the
    /// terminal.
    pub fn resolve(&self, ctx: KeyContext, key: KeyPress) -> Resolution {
        let key = key.normalized();
        for table in self.tables(ctx) {
            // A chord prefix in a higher-ranked table still loses to a complete
            // binding in the same table, but wins over anything below it.
            match table.resolve(key) {
                Resolution::Unbound => {}
                found => return found,
            }
        }
        Resolution::Unbound
    }

    /// Complete a chord started by [`Resolution::ChordPending`].
    pub fn resolve_chord(
        &self,
        ctx: KeyContext,
        first: KeyPress,
        second: KeyPress,
    ) -> Option<Action> {
        let (first, second) = (first.normalized(), second.normalized());
        self.tables(ctx)
            .into_iter()
            .find_map(|table| table.resolve_chord(first, second))
    }

    /// Every binding of an action, for the `F1` keyboard reference.
    /// Order is unspecified.
    pub fn bindings_for(&self, action: Action) -> Vec<Binding> {
        let mut out = Vec::new();
        self.user.bindings_for(action, &mut out);
        self.builtin.bindings_for(action, &mut out);
        out
    }

    /// Every binding of an action within one context, plus the global ones.
    pub fn bindings_in(&self, ctx: KeyContext, action: Action) -> Vec<Binding> {
        let mut out = Vec::new();
        for table in self.tables(ctx) {
            table.bindings_for(action, &mut out);
        }
        out
    }

    /// How a binding is written in a menu row and on the `F1` page:
    /// `F5`, `Ctrl+Shift+3`, `Alt+F1 / Alt+D`, `(unbound)`.
    ///
    /// One definition, so the page and the menu cannot
    /// spell the same key two ways.
    ///
    /// Bindings the terminal cannot deliver are marked with [`UNAVAILABLE`]
    /// when `enhanced` is false, and every other binding of the same action
    /// stays in the list beside them - which is how the "the
    /// working fallback shown next to them" is delivered without a second
    /// lookup. An action whose *only* binding is undeliverable says so
    /// instead, because there is nothing to show beside it and a row that
    /// simply said `(unavailable)` would leave the reader guessing whether a
    /// fallback exists.
    pub fn describe(&self, ctx: KeyContext, action: Action, enhanced: bool) -> String {
        let mut bindings = self.bindings_in(ctx, action);
        // The same binding can appear in more than one table - the user's
        // context table over the built-in global one, most often - and the
        // page must not print it twice. `dedup` and not a sort: the order the
        // tables are consulted in is the order the design resolves them in,
        // and the first entry is the one that wins.
        bindings.dedup();
        if bindings.is_empty() {
            return UNBOUND.to_string();
        }
        let mut deliverable = false;
        let mut parts: Vec<String> = Vec::with_capacity(bindings.len());
        for binding in &bindings {
            let text = pretty_binding(*binding);
            if !enhanced && binding_is_undeliverable(*binding) {
                parts.push(format!("{text} {UNAVAILABLE}"));
            } else {
                deliverable = true;
                parts.push(text);
            }
        }
        if !deliverable {
            parts.push(NO_FALLBACK.to_string());
        }
        parts.join(" / ")
    }

    /// The full compiled-in Total Commander layout, so a user with
    /// no `keymap.toml` gets everything.
    ///
    /// It is `examples/keymap.toml`, embedded, plus the keys the design and the
    /// design describe in prose rather than in the example file: cursor
    /// movement, caret movement, and `Shift+Enter` / `Ctrl+Shift+Enter`.
    pub fn builtin() -> Self {
        let mut km = Self::default();
        let embedded = include_str!("../../examples/keymap.toml");
        km.overlay_into(LayerId::Builtin, embedded, "built-in keymap");
        // The embedded file is ours; a warning here is a bug in the repository,
        // not in the user's configuration, so it is surfaced rather than hidden.
        //
        // **After the file, not before it.** `overlay_into` replaces rather
        // than adds: for every action a table mentions it first removes *every*
        // binding that action already has. Applied to defaults installed into
        // the same layer, that is collateral - `[cmdline] line_start =
        // ["ctrl+a"]` used to take `Home` with it, and `line_end` took `End`,
        // leaving both silently unbound on the command line of the design
        // that has no shell to forward them to. Installing the prose defaults
        // last, into the gaps the file left, keeps the file's replacements
        // exactly as written and the prose defaults exactly as specified.
        km.install_prose_defaults();
        km
    }

    /// The bindings the design specify in prose and the example keymap
    /// leaves out, because they are not things anyone rebinds.
    ///
    /// Fills gaps: a key the embedded `keymap.toml` has already claimed in the
    /// same context keeps what the file gave it. See [`Keymap::builtin`].
    fn install_prose_defaults(&mut self) {
        use Action as A;
        use KeyContext::{CmdLine, Panel, Viewer};

        let k = |code: KeyCode| Binding::Key(KeyPress::plain(code));
        let m = |code: KeyCode, mods: KeyModifiers| Binding::Key(KeyPress::new(code, mods));
        // These are defaults, so they go in the built-in layer (the design
        // step 3) where a user's keymap.toml outranks them - and only where the
        // embedded file has not already spoken for the key.
        let mut bind = |ctx: KeyContext, b: Binding, a: Action| {
            let table = self.builtin.ctx_mut(ctx);
            if !table.claims(b) {
                table.insert(b, a);
            }
        };

        // Panel cursor movement. Clears the quick-search buffer.
        for (b, a) in [
            (k(KeyCode::Up), A::CursorUp),
            (k(KeyCode::Down), A::CursorDown),
            (k(KeyCode::PageUp), A::CursorPageUp),
            (k(KeyCode::PageDown), A::CursorPageDown),
            (k(KeyCode::Home), A::CursorTop),
            (k(KeyCode::End), A::CursorBottom),
        ] {
            bind(Panel, b, a);
        }

        // Shift+Enter always opens with the association.
        bind(Panel, m(KeyCode::Enter, KeyModifiers::SHIFT), A::OpenWith);
        // Ctrl+Shift+Enter inserts the full path.
        //
        // A legacy terminal can deliver neither ctrl+enter nor ctrl+shift+enter
        // (the design lists the first; the second is a superset of it), and
        // alt+shift+enter is no better - without the enhanced protocol
        // shift+enter is a bare CR, so it would arrive as alt+enter, which is
        // already put_selected's fallback. item 2 requires *a* documented
        // alternate binding, so the fallback is `alt+y` - a single ESC-prefixed
        // byte, which every terminal delivers. Deliberately not a `ctrl+x`
        // chord: `ctrl+x` is cut and a chord prefix is an mc idiom the design
        // rules out.
        let chord_x_p = Binding::Key(KeyPress::new(KeyCode::Char('y'), KeyModifiers::ALT));
        for ctx in [Panel, CmdLine] {
            bind(
                ctx,
                m(KeyCode::Enter, KeyModifiers::CONTROL | KeyModifiers::SHIFT),
                A::PutSelectedPath,
            );
            bind(ctx, chord_x_p, A::PutSelectedPath);
        }

        // Command-line editing.
        for (b, a) in [
            (k(KeyCode::Left), A::CaretLeft),
            (k(KeyCode::Right), A::CaretRight),
            (k(KeyCode::Backspace), A::CaretBackspace),
            (k(KeyCode::Delete), A::CaretDelete),
            (k(KeyCode::Insert), A::ToggleOverwrite),
            (k(KeyCode::Home), A::LineStart),
            (k(KeyCode::End), A::LineEnd),
        ] {
            bind(CmdLine, b, a);
        }

        // Viewer movement reuses the panel cursor actions, and the command
        // line's caret actions for the horizontal half.
        //
        // the design lists "arrows" - all four of them - and `Left`/`Right`
        // are the two the viewer cannot do without: with wrap off they are how
        // the rest of a long line is reached (the "optional wrap"),
        // and in hex they move the cursor a *byte* at a time, which is what
        // makes the "the current offset under the cursor is in the
        // status line" say anything at all. Without them the cursor can only
        // ever be a row start and `Viewer::scroll_horizontal` is reachable from
        // no key.
        for (b, a) in [
            (k(KeyCode::Up), A::CursorUp),
            (k(KeyCode::Down), A::CursorDown),
            (k(KeyCode::Left), A::CaretLeft),
            (k(KeyCode::Right), A::CaretRight),
            (k(KeyCode::PageUp), A::CursorPageUp),
            (k(KeyCode::PageDown), A::CursorPageDown),
            // bare `Home`/`End` are the *line's* edges, and the file's are
            // `Ctrl`ed. The plain keys used to be the file's, which was only
            // tenable while the viewer had no cursor to put anywhere but the
            // top left.
            (k(KeyCode::Home), A::LineStart),
            (k(KeyCode::End), A::LineEnd),
            (m(KeyCode::Home, KeyModifiers::CONTROL), A::CursorTop),
            (m(KeyCode::End, KeyModifiers::CONTROL), A::CursorBottom),
            // `Ctrl` with a movement scrolls the view and
            // leaves the cursor and the selection alone. These are not
            // movements, so `viewer_extend` still reads `Ctrl+Shift+Up` as a
            // rectangular extension rather than as a scroll.
            (m(KeyCode::Up, KeyModifiers::CONTROL), A::ViewScrollUp),
            (m(KeyCode::Down, KeyModifiers::CONTROL), A::ViewScrollDown),
            (
                m(KeyCode::PageUp, KeyModifiers::CONTROL),
                A::ViewScrollPageUp,
            ),
            (
                m(KeyCode::PageDown, KeyModifiers::CONTROL),
                A::ViewScrollPageDown,
            ),
            (m(KeyCode::Left, KeyModifiers::CONTROL), A::ViewScrollLeft),
            (m(KeyCode::Right, KeyModifiers::CONTROL), A::ViewScrollRight),
        ] {
            bind(Viewer, b, a);
        }
    }

    /// Lay a `keymap.toml` over this map, replacing the bindings of every
    /// action the file mentions. Warnings are appended to [`Keymap::warnings`].
    pub fn overlay(&mut self, text: &str, file_label: &str) {
        self.overlay_into(LayerId::User, text, file_label);
    }

    /// [`Keymap::overlay`], into a named layer. Loading the compiled-in
    /// defaults uses [`LayerId::Builtin`]; everything else is the user layer.
    fn overlay_into(&mut self, layer: LayerId, text: &str, file_label: &str) {
        let doc: toml::Table = match toml::from_str(text) {
            Ok(doc) => doc,
            Err(err) => {
                self.warnings.push(format!(
                    "{file_label}: {err}; keeping the built-in key bindings"
                ));
                return;
            }
        };

        // Every (context, key) this file has already claimed. A second claim
        // is the user's own conflict; displacing a *built-in* default is the
        // documented way to rebind and is not warned about.
        let mut claimed: std::collections::HashSet<(Option<KeyContext>, Binding)> =
            std::collections::HashSet::new();

        for (table_name, value) in &doc {
            let ctx = if table_name == "global" {
                None
            } else if let Some(c) = KeyContext::from_id(table_name) {
                Some(c)
            } else {
                self.warnings.push(format!(
                    "{file_label}: unknown section [{table_name}] (known: global, {})",
                    KeyContext::ALL
                        .iter()
                        .map(|c| c.id())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                continue;
            };

            let Some(table) = value.as_table() else {
                self.warnings
                    .push(format!("{file_label}: [{table_name}] is not a table"));
                continue;
            };

            for (id, bindings) in table {
                let Some(action) = Action::from_id(id) else {
                    self.warnings.push(format!(
                        "{file_label}: unknown action {id:?} in [{table_name}]"
                    ));
                    continue;
                };
                let Some(list) = bindings.as_array() else {
                    self.warnings.push(format!(
                        "{file_label}: {id} in [{table_name}] must be a list of bindings"
                    ));
                    continue;
                };

                // Replace, do not add. See the module docs: the built-in
                // bindings of a mentioned action go first, in this same table,
                // so `quit = ["ctrl+q"]` means quit is Ctrl+Q and nothing else.
                self.layer_mut(layer).table_mut(ctx).remove_action(action);
                if layer == LayerId::User {
                    self.builtin.table_mut(ctx).remove_action(action);
                }

                for item in list {
                    let Some(text) = item.as_str() else {
                        self.warnings.push(format!(
                            "{file_label}: a binding for {id} in [{table_name}] is not a string"
                        ));
                        continue;
                    };
                    match Binding::parse(text) {
                        Ok(binding) => {
                            if let Some(displaced) =
                                self.bind_reporting(layer, ctx, binding, action)
                                && claimed.contains(&(ctx, binding))
                            {
                                // Both actions come from this file, so the user
                                // wrote a genuine conflict rather than
                                // overriding a built-in default on purpose.
                                self.warnings.push(format!(
                                    "{file_label}: [{table_name}] binds {text:?} to both \
                                     {:?} and {id:?}; {id:?} wins",
                                    displaced.id()
                                ));
                            }
                            claimed.insert((ctx, binding));
                        }
                        Err(err) => self
                            .warnings
                            .push(format!("{file_label}: {id} in [{table_name}]: {err}")),
                    }
                }
            }
        }
    }

    /// Load a user `keymap.toml` on top of the built-in layout.
    pub fn load(text: &str, file_label: &str) -> Self {
        let mut km = Self::builtin();
        km.overlay(text, file_label);
        km
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_viewer_hex_keys_resolve() {
        let km = Keymap::builtin();
        assert!(km.warnings.is_empty(), "builtin warns: {:#?}", km.warnings);
        for (ch, want) in [
            ('g', Action::HexGroup),
            ('d', Action::HexFormat),
            ('e', Action::HexEndian),
            ('w', Action::ToggleWrap),
        ] {
            assert_eq!(
                km.resolve(
                    KeyContext::Viewer,
                    press(KeyCode::Char(ch), KeyModifiers::NONE)
                ),
                Resolution::Action(want),
                "{ch:?}"
            );
        }
        assert_eq!(
            km.resolve(KeyContext::Viewer, press(KeyCode::F(2), KeyModifiers::NONE)),
            Resolution::Action(Action::ViewerReload)
        );
    }

    use super::*;
    use crate::input::parse_key;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn press(code: KeyCode, mods: KeyModifiers) -> KeyPress {
        KeyPress::new(code, mods)
    }

    #[test]
    fn the_builtin_keymap_loads_without_warnings() {
        let km = Keymap::builtin();
        assert!(km.warnings.is_empty(), "{:?}", km.warnings);
    }

    #[test]
    fn the_prose_defaults_survive_the_embedded_keymap() {
        // `overlay_into` replaces rather than adds: for every action a table
        // mentions it first removes *every* binding that action already has.
        // Applied to the defaults the design give in prose and installed into
        // the same layer, that was collateral - `[cmdline] line_start =
        // ["ctrl+a"]` took `Home` with it and `line_end` took `End`, leaving
        // both silently unbound on the command line. With a shell those keys
        // are forwarded (`Unbound` means the shell's) so nothing showed; on the
        // command line that has no shell they simply did nothing.
        let km = Keymap::builtin();
        for (code, action) in [
            (KeyCode::Home, Action::LineStart),
            (KeyCode::End, Action::LineEnd),
            (KeyCode::Left, Action::CaretLeft),
            (KeyCode::Right, Action::CaretRight),
            (KeyCode::Backspace, Action::CaretBackspace),
            (KeyCode::Delete, Action::CaretDelete),
        ] {
            assert_eq!(
                km.resolve(KeyContext::CmdLine, press(code, KeyModifiers::NONE)),
                Resolution::Action(action),
                "{code:?} on the command line"
            );
        }
        // And what the file *did* say still wins where it said it.
        assert_eq!(
            km.resolve(
                KeyContext::CmdLine,
                press(KeyCode::Char('a'), KeyModifiers::CONTROL)
            ),
            Resolution::Action(Action::LineStart)
        );
        assert_eq!(
            km.resolve(KeyContext::CmdLine, press(KeyCode::Up, KeyModifiers::NONE)),
            Resolution::Action(Action::LeaveToPanel),
            "the file's binding is not displaced by a prose default"
        );
        // The panel keeps its own. The viewer's bare `Home` is the *line's*
        // start and the file's is `Ctrl+Home`.
        assert_eq!(
            km.resolve(KeyContext::Panel, press(KeyCode::Home, KeyModifiers::NONE)),
            Resolution::Action(Action::CursorTop)
        );
        assert_eq!(
            km.resolve(KeyContext::Viewer, press(KeyCode::Home, KeyModifiers::NONE)),
            Resolution::Action(Action::LineStart)
        );
    }

    #[test]
    fn every_arrow_navigates_the_viewer() {
        // the design lists "arrows … navigate", all four of them, and the
        // horizontal pair is the half that is easy to leave out: `Left` and
        // `Right` are the *command line's* caret actions, so nothing about the
        // viewer's `[viewer]` table mentions them. Without this binding
        // `Viewer::scroll_horizontal` is reachable from no key at all - the
        // rest of a long line cannot be read with wrap off, and the design's
        // "the current offset under the cursor is in the status line" is a
        // sentence about a cursor that can only ever sit on a row start.
        let km = Keymap::builtin();
        for (code, action) in [
            (KeyCode::Up, Action::CursorUp),
            (KeyCode::Down, Action::CursorDown),
            (KeyCode::Left, Action::CaretLeft),
            (KeyCode::Right, Action::CaretRight),
            (KeyCode::PageUp, Action::CursorPageUp),
            (KeyCode::PageDown, Action::CursorPageDown),
            // the line's edges bare, the file's with `Ctrl`.
            (KeyCode::Home, Action::LineStart),
            (KeyCode::End, Action::LineEnd),
        ] {
            assert_eq!(
                km.resolve(KeyContext::Viewer, press(code, KeyModifiers::NONE)),
                Resolution::Action(action),
                "{code:?} in the viewer"
            );
        }
        for (code, action) in [
            (KeyCode::Home, Action::CursorTop),
            (KeyCode::End, Action::CursorBottom),
        ] {
            assert_eq!(
                km.resolve(KeyContext::Viewer, press(code, KeyModifiers::CONTROL)),
                Resolution::Action(action),
                "Ctrl+{code:?} seeks to the first or last page"
            );
        }
    }

    #[test]
    fn the_total_commander_function_keys_are_there() {
        let km = Keymap::builtin();
        for (n, action) in [
            (1u8, Action::Help),
            (3, Action::View),
            (4, Action::Edit),
            (5, Action::Copy),
            (6, Action::Move),
            (7, Action::Mkdir),
            (8, Action::Delete),
            (9, Action::Menu),
            (10, Action::Quit),
        ] {
            assert_eq!(
                km.resolve(KeyContext::Panel, KeyPress::plain(KeyCode::F(n))),
                Resolution::Action(action),
                "F{n}"
            );
        }
    }

    #[test]
    fn context_wins_over_global() {
        // ctrl+w kills a word on the command line and closes a tab on a panel
        // (resolved by context).
        let km = Keymap::builtin();
        let ctrl_w = press(KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(
            km.resolve(KeyContext::CmdLine, ctrl_w),
            Resolution::Action(Action::KillWord)
        );
        assert_eq!(
            km.resolve(KeyContext::Panel, ctrl_w),
            Resolution::Action(Action::TabClose)
        );
    }

    #[test]
    fn a_user_global_binding_outranks_a_builtin_context_binding() {
        // step 2 (global from keymap.toml) is above step 3
        // (built-in default for the context). A minimal hand-written file that
        // rebinds hotlist to backspace must win over the built-in
        // [panel] parent = ["backspace"], or the user's own file is dead.
        let km = Keymap::load("[global]\nhotlist = [\"backspace\"]\n", "test");
        assert!(km.warnings.is_empty(), "{:?}", km.warnings);
        let backspace = KeyPress::plain(KeyCode::Backspace);
        assert_eq!(
            km.resolve(KeyContext::Panel, backspace),
            Resolution::Action(Action::Hotlist)
        );
        // The rebinding also replaced the built-in ctrl+d rather than adding
        // to it (the layering rule in the module docs).
        assert_eq!(
            km.resolve(
                KeyContext::Panel,
                press(KeyCode::Char('d'), KeyModifiers::CONTROL)
            ),
            Resolution::Unbound
        );
        // A [global] line means global: it outranks the built-in context
        // default in every context, including the command line's backspace.
        // That is the point of writing it in [global] rather than in [panel].
        assert_eq!(
            km.resolve(KeyContext::CmdLine, backspace),
            Resolution::Action(Action::Hotlist)
        );
    }

    #[test]
    fn a_user_context_binding_outranks_a_user_global_one() {
        // Step 1 above step 2, still.
        let km = Keymap::load(
            "[global]\nhotlist = [\"ctrl+y\"]\n\n[panel]\nroot = [\"ctrl+y\"]\n",
            "test",
        );
        assert!(km.warnings.is_empty(), "{:?}", km.warnings);
        let ctrl_y = press(KeyCode::Char('y'), KeyModifiers::CONTROL);
        assert_eq!(
            km.resolve(KeyContext::Panel, ctrl_y),
            Resolution::Action(Action::Root)
        );
        assert_eq!(
            km.resolve(KeyContext::CmdLine, ctrl_y),
            Resolution::Action(Action::Hotlist)
        );
    }

    #[test]
    fn a_user_file_replaces_the_builtin_bindings_of_the_action_it_mentions() {
        let km = Keymap::load("[panel]\nparent = [\"ctrl+pgup\"]\n", "test");
        assert!(km.warnings.is_empty(), "{:?}", km.warnings);
        // backspace was one of parent's built-in bindings and is gone, so it
        // falls through to the panel's default text handling.
        assert_eq!(
            km.resolve(KeyContext::Panel, KeyPress::plain(KeyCode::Backspace)),
            Resolution::Unbound
        );
        assert_eq!(
            km.resolve(
                KeyContext::Panel,
                press(KeyCode::PageUp, KeyModifiers::CONTROL)
            ),
            Resolution::Action(Action::Parent)
        );
    }

    #[test]
    fn put_selected_path_has_a_legacy_terminal_fallback() {
        // item 2: every key the protocol denies gets a documented
        // alternate binding, and every fallback is a single alt+letter.
        let km = Keymap::builtin();
        for ctx in [KeyContext::Panel, KeyContext::CmdLine] {
            assert_eq!(
                km.resolve(
                    ctx,
                    press(KeyCode::Enter, KeyModifiers::CONTROL | KeyModifiers::SHIFT)
                ),
                Resolution::Action(Action::PutSelectedPath),
                "{ctx:?}"
            );
            assert_eq!(
                km.resolve(ctx, press(KeyCode::Char('y'), KeyModifiers::ALT)),
                Resolution::Action(Action::PutSelectedPath),
                "{ctx:?}"
            );
        }
    }

    #[test]
    fn ctrl_x_is_cut_and_not_a_chord_prefix() {
        // ctrl+x is Total Commander's cut, and a chord
        // prefix is a Midnight Commander idiom this project does not inherit.
        let km = Keymap::builtin();
        let ctrl_x = press(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_ne!(
            km.resolve(KeyContext::Panel, ctrl_x),
            Resolution::ChordPending,
            "ctrl+x must not begin a chord"
        );
        // And the fallbacks it used to carry are single alt+letter keys now.
        for (ch, action) in [
            ('d', Action::DriveLeft),
            ('g', Action::DriveRight),
            ('s', Action::Search),
        ] {
            assert_eq!(
                km.resolve(
                    KeyContext::Panel,
                    press(KeyCode::Char(ch), KeyModifiers::ALT)
                ),
                Resolution::Action(action),
                "alt+{ch}"
            );
        }
    }

    #[test]
    fn chords_resolve_in_two_presses() {
        // Nothing shipped uses a chord any more - the fallbacks are single
        // alt+letter keys and the design gives ctrl+x back to cut. The
        // machinery stays because `[terminal.sequences]` may want it, so it is
        // exercised against a chord bound here rather than one in the keymap.
        let mut km = Keymap::builtin();
        let prefix = press(KeyCode::Char('k'), KeyModifiers::CONTROL);
        km.bind(
            Some(KeyContext::Panel),
            Binding::Chord(prefix, KeyPress::plain(KeyCode::Char('z'))),
            Action::SelectAll,
        );
        assert_eq!(
            km.resolve(KeyContext::Panel, prefix),
            Resolution::ChordPending
        );
        assert_eq!(
            km.resolve_chord(
                KeyContext::Panel,
                prefix,
                KeyPress::plain(KeyCode::Char('z'))
            ),
            Some(Action::SelectAll)
        );
        assert_eq!(
            km.resolve_chord(
                KeyContext::Panel,
                prefix,
                KeyPress::plain(KeyCode::Char('q'))
            ),
            None,
            "an unbound second press completes to nothing"
        );
    }

    #[test]
    fn a_printable_key_is_unbound_so_it_reaches_quick_search() {
        let km = Keymap::builtin();
        for c in ['t', 'h', 'o', '2', '0'] {
            assert_eq!(
                km.resolve(KeyContext::Panel, KeyPress::plain(KeyCode::Char(c))),
                Resolution::Unbound,
                "{c} must fall through to quick search"
            );
        }
    }

    #[test]
    fn alt_digits_switch_tabs_and_bare_digits_do_not() {
        let km = Keymap::builtin();
        assert_eq!(
            km.resolve(
                KeyContext::Panel,
                press(KeyCode::Char('3'), KeyModifiers::ALT)
            ),
            Resolution::Action(Action::Tab3)
        );
        assert_eq!(
            km.resolve(
                KeyContext::Panel,
                press(KeyCode::Char('3'), KeyModifiers::CONTROL)
            ),
            Resolution::Action(Action::SortByColumn3)
        );
    }

    #[test]
    fn a_user_file_replaces_an_actions_bindings_rather_than_adding() {
        let km = Keymap::load("[global]\nquit = [\"ctrl+q\"]\n", "user");
        assert!(km.warnings.is_empty(), "{:?}", km.warnings);
        assert_eq!(
            km.resolve(
                KeyContext::Panel,
                press(KeyCode::Char('q'), KeyModifiers::CONTROL)
            ),
            Resolution::Action(Action::Quit)
        );
        // alt+q was a built-in binding for quit and is gone.
        assert_eq!(
            km.resolve(
                KeyContext::Panel,
                press(KeyCode::Char('q'), KeyModifiers::ALT)
            ),
            Resolution::Unbound
        );
        // An action the file did not mention keeps its defaults.
        assert_eq!(
            km.resolve(KeyContext::Panel, KeyPress::plain(KeyCode::F(5))),
            Resolution::Action(Action::Copy)
        );
    }

    #[test]
    fn a_broken_user_file_warns_and_keeps_the_defaults() {
        let km = Keymap::load(
            "[global]\nquit = [\"ctrl+nonsense\"]\nnot_an_action = [\"f1\"]\n[nope]\nx = [\"f2\"]\n",
            "user.toml",
        );
        assert_eq!(km.warnings.len(), 3, "{:?}", km.warnings);
        assert!(km.warnings.iter().all(|w| w.contains("user.toml")));
        // quit lost its bindings because the file mentioned it and every
        // binding it gave was bad. That is the user's file being wrong, said
        // out loud, rather than silently ignored.
        assert_eq!(
            km.resolve(KeyContext::Panel, KeyPress::plain(KeyCode::F(5))),
            Resolution::Action(Action::Copy)
        );
    }

    #[test]
    fn every_action_bound_by_the_shipped_keymap_is_findable() {
        let km = Keymap::builtin();
        assert!(!km.bindings_for(Action::Quit).is_empty());
        assert!(!km.bindings_for(Action::DriveLeft).is_empty());
        assert!(
            km.bindings_for(Action::Quit).len() >= 3,
            "f10, alt+q, alt+f4"
        );
    }

    #[test]
    fn the_shipped_keymap_loads_without_a_single_warning() {
        // `Keymap::builtin` overlays `examples/keymap.toml`, and
        // `ensure_default_files` writes that same file into the user's config
        // directory on first run - so a conflict in it is a conflict everybody
        // gets. Three were found this way: `leave_virtual` claiming `esc` and
        // `ctrl+r` out from under `clear_search` and `reread`, and `f3` bound
        // to both `close` and `find_next` in the viewer.
        let km = Keymap::builtin();
        assert!(
            km.warnings.is_empty(),
            "the built-in keymap warns: {:#?}",
            km.warnings
        );

        let km = Keymap::load(crate::config::EXAMPLE_KEYMAP, "keymap.toml");
        assert!(
            km.warnings.is_empty(),
            "examples/keymap.toml warns: {:#?}",
            km.warnings
        );

        // The shipped file as a *user* file is the ordinary case, since
        // `ensure_default_files` writes it into the config directory. The
        // built-in layer underneath still has to work: `alt+y`, the fallback
        // for `put_selected_path`, lives only in the built-in [panel] table and
        // must survive a user file that never mentions it.
        assert_eq!(
            km.resolve(
                KeyContext::Panel,
                press(KeyCode::Char('y'), KeyModifiers::ALT)
            ),
            Resolution::Action(Action::PutSelectedPath)
        );
        // And ctrl+x is cut, not a chord prefix.
        assert_eq!(
            km.resolve(
                KeyContext::Panel,
                press(KeyCode::Char('x'), KeyModifiers::CONTROL)
            ),
            Resolution::Action(Action::ClipboardCut)
        );
    }

    #[test]
    fn esc_on_a_panel_clears_the_search_and_ctrl_r_rereads() {
        // The keys the design gives a second, state-dependent meaning to.
        // Neither may be swallowed by a `leave_virtual` binding.
        let km = Keymap::builtin();
        assert_eq!(
            km.resolve(KeyContext::Panel, KeyPress::plain(KeyCode::Esc)),
            Resolution::Action(Action::ClearSearch)
        );
        assert_eq!(
            km.resolve(
                KeyContext::Panel,
                KeyPress::new(KeyCode::Char('r'), KeyModifiers::CONTROL)
            ),
            Resolution::Action(Action::Reread)
        );
    }

    #[test]
    fn shift_plus_a_letter_is_a_different_key_from_the_letter() {
        // `shift+n` is spellable and is not `n` (the design needs both).
        let n = parse_key("n").expect("n");
        let shift_n = parse_key("shift+n").expect("shift+n");
        assert_ne!(n, shift_n);
        // And it is what a terminal actually delivers for Shift+N.
        assert_eq!(
            shift_n,
            KeyPress::new(KeyCode::Char('N'), KeyModifiers::SHIFT).normalized()
        );
        // Punctuation keeps the old rule: which character arrived is the
        // shifted state, so `shift+plus` is still just `plus`.
        assert_eq!(
            parse_key("shift+plus").expect("shift+plus"),
            parse_key("plus").expect("plus")
        );
    }
    #[test]
    fn every_action_is_named_in_the_example_keymap() {
        // The file is both the built-in layer and the reference written into
        // the user's config directory, so an action missing
        // from it is an action nobody can discover or rebind by reading. Seven
        // of the panel's cursor keys and six of the command line's caret keys
        // used to be exactly that: bound in `install_prose_defaults` and named
        // nowhere.
        let text = include_str!("../../examples/keymap.toml");
        let named: std::collections::HashSet<&str> = text
            .lines()
            .map(str::trim_start)
            .filter_map(|l| l.strip_prefix('#').unwrap_or(l).split('=').next())
            .map(str::trim)
            .collect();
        let mut missing: Vec<&str> = Action::ALL
            .iter()
            .map(|a| a.id())
            .filter(|id| !named.contains(id))
            .collect();
        missing.sort_unstable();
        assert!(
            missing.is_empty(),
            "actions absent from examples/keymap.toml: {missing:?}"
        );
    }

    #[test]
    fn listing_the_prose_defaults_did_not_move_them() {
        // They were moved from `install_prose_defaults` into the file so they
        // could be read and rebound. `install_prose_defaults` fills gaps only,
        // so the file now supplies them - and these are the values it has to
        // supply for that to be a no-op rather than a change.
        let km = Keymap::builtin();
        let cases: &[(KeyContext, KeyCode, KeyModifiers, Action)] = &[
            (
                KeyContext::Panel,
                KeyCode::Up,
                KeyModifiers::NONE,
                Action::CursorUp,
            ),
            (
                KeyContext::Panel,
                KeyCode::Down,
                KeyModifiers::NONE,
                Action::CursorDown,
            ),
            (
                KeyContext::Panel,
                KeyCode::PageUp,
                KeyModifiers::NONE,
                Action::CursorPageUp,
            ),
            (
                KeyContext::Panel,
                KeyCode::PageDown,
                KeyModifiers::NONE,
                Action::CursorPageDown,
            ),
            (
                KeyContext::Panel,
                KeyCode::Home,
                KeyModifiers::NONE,
                Action::CursorTop,
            ),
            (
                KeyContext::Panel,
                KeyCode::End,
                KeyModifiers::NONE,
                Action::CursorBottom,
            ),
            (
                KeyContext::CmdLine,
                KeyCode::Left,
                KeyModifiers::NONE,
                Action::CaretLeft,
            ),
            (
                KeyContext::CmdLine,
                KeyCode::Right,
                KeyModifiers::NONE,
                Action::CaretRight,
            ),
            (
                KeyContext::CmdLine,
                KeyCode::Backspace,
                KeyModifiers::NONE,
                Action::CaretBackspace,
            ),
            (
                KeyContext::CmdLine,
                KeyCode::Delete,
                KeyModifiers::NONE,
                Action::CaretDelete,
            ),
            (
                KeyContext::CmdLine,
                KeyCode::Insert,
                KeyModifiers::NONE,
                Action::ToggleOverwrite,
            ),
            // The viewer's, where the design gives home/end and ctrl+home/
            // ctrl+end different jobs from the panel's.
            (
                KeyContext::Viewer,
                KeyCode::Up,
                KeyModifiers::NONE,
                Action::CursorUp,
            ),
            (
                KeyContext::Viewer,
                KeyCode::Left,
                KeyModifiers::NONE,
                Action::CaretLeft,
            ),
            (
                KeyContext::Viewer,
                KeyCode::Home,
                KeyModifiers::CONTROL,
                Action::CursorTop,
            ),
            (
                KeyContext::Viewer,
                KeyCode::End,
                KeyModifiers::CONTROL,
                Action::CursorBottom,
            ),
            (
                KeyContext::Viewer,
                KeyCode::Home,
                KeyModifiers::NONE,
                Action::LineStart,
            ),
            (
                KeyContext::Viewer,
                KeyCode::End,
                KeyModifiers::NONE,
                Action::LineEnd,
            ),
        ];
        for (ctx, code, mods, action) in cases {
            assert_eq!(
                km.resolve(*ctx, press(*code, *mods)),
                Resolution::Action(*action),
                "{ctx:?} {code:?} {mods:?}"
            );
        }
    }
}
