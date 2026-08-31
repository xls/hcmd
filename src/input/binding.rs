//! Key-binding strings.
//!
//! A binding is either a single key press - `"ctrl+alt+r"`, `"shift+f10"`,
//! `"alt+period"` - or a two-key chord written with a space: `"ctrl+x l"`.
//! Chords exist because a legacy terminal cannot deliver `Alt+F1`,
//! and `ctrl+x l` can be typed anywhere.
//!
//! # Normalisation
//!
//! Terminals disagree about whether `SHIFT` is reported for a shifted
//! character. With the Kitty protocol, `+` may arrive as `Char('+')` with
//! `SHIFT` set; without it, as `Char('+')` with no modifiers. So every key
//! press - parsed from a binding string or read from the terminal - goes
//! through [`KeyPress::normalized`] before it is compared:
//!
//! * an uppercase `Char` becomes lowercase with `SHIFT` set, so `ctrl+a` and
//!   `Ctrl+A` are the same binding;
//! * any other `Char` has `SHIFT` cleared, because for punctuation and digits
//!   the shifted state is already expressed by *which* character arrived;
//! * `SUPER`, `HYPER` and `META` are dropped, since they only appear under the
//!   enhanced protocol and nothing binds them.

use std::fmt;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::error::{Error, Result};

/// One key press: a code plus the modifiers that were held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyPress {
    /// The key.
    pub code: KeyCode,
    /// The modifiers.
    pub mods: KeyModifiers,
}

impl KeyPress {
    /// From parts.
    pub const fn new(code: KeyCode, mods: KeyModifiers) -> Self {
        Self { code, mods }
    }

    /// A printable key with no modifier but `Shift`: text, in other words.
    ///
    /// What the command line must never let a fallback swallow. `Space` is the
    /// panel's `select_and_size` and `d` is nothing at all, but on the command
    /// line both are characters somebody is typing, and a key that inserts a
    /// character cannot also be a command.
    #[must_use]
    pub fn is_bare_character(self) -> bool {
        matches!(self.code, KeyCode::Char(_)) && (self.mods - KeyModifiers::SHIFT).is_empty()
    }

    /// An unmodified key.
    pub const fn plain(code: KeyCode) -> Self {
        Self::new(code, KeyModifiers::NONE)
    }

    /// The canonical form used for every comparison. See the module docs.
    pub fn normalized(self) -> Self {
        let mut mods = self.mods;
        mods.remove(KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META);
        match self.code {
            // The fold is applied only where it is *reversible*: `as_text`
            // reconstructs the typed character by upper-casing the folded one,
            // and case mapping is not one-to-one for every letter. Turkish `İ`
            // (U+0130) lower-cases to two code points, and `ẞ` (U+1E9E)
            // lower-cases to `ß`, which upper-cases to `SS` - fold either and
            // the character the user actually pressed is replaced by a
            // different one, in the quick-search buffer and in
            // the command line alike. Those keys keep their own
            // code point instead, and lose `SHIFT` like any other character
            // that already carries its shifted state.
            KeyCode::Char(c) if c.is_uppercase() => match fold_case(c) {
                Some(lower) => {
                    mods.insert(KeyModifiers::SHIFT);
                    Self::new(KeyCode::Char(lower), mods)
                }
                None => {
                    mods.remove(KeyModifiers::SHIFT);
                    Self::new(self.code, mods)
                }
            },
            KeyCode::Char(c) => {
                // `SHIFT` is dropped only where the press would *type*
                // something that already carries its shifted state: `!` is
                // what `shift+1` types, so keeping the flag would make
                // `shift+1` a second, unreachable spelling of it. With
                // `CONTROL` or `ALT` held nothing is typed, so `SHIFT` is a
                // modifier in its own right - and `Ctrl+Shift+3` has to stay
                // distinct from `Ctrl+3`, which is what the secondary sort is
                // bound to.
                //
                // **A folded letter keeps it**, and that is what makes this
                // function idempotent. The arm above turns `N` into
                // `('n', SHIFT)`; stripping `SHIFT` here would turn it into
                // plain `n` the *second* time normalisation ran - and it runs
                // twice on every keystroke, once in `From<KeyEvent>` and once
                // inside `Keymap::resolve`. The visible cost of that was that
                // `[viewer] find_prev = ["shift+n"]` (the design's
                // `Shift+N`) resolved to `find_next` and the only way back
                // through a file's matches did not exist.
                if !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && unfold_case(c).is_none()
                {
                    mods.remove(KeyModifiers::SHIFT);
                }
                Self::new(self.code, mods)
            }
            KeyCode::BackTab => {
                // BackTab *is* shift+tab; spell it that way so `"shift+tab"`
                // binds on both legacy and enhanced terminals.
                mods.insert(KeyModifiers::SHIFT);
                Self::new(KeyCode::Tab, mods)
            }
            _ => Self::new(self.code, mods),
        }
    }

    /// The character this press would type, if it would type one.
    ///
    /// `None` whenever `CONTROL` or `ALT` is held, so a bound `Ctrl+K` never
    /// leaks into the quick-search buffer.
    ///
    /// Normalisation folds an uppercase `Char` to lowercase plus `SHIFT` so
    /// that `ctrl+a` and `Ctrl+A` are one binding - but the *text* a press
    /// types must keep its case, or smart-case quick search could never see an
    /// uppercase character and `Tho` would match `thorin`. So
    /// the fold is undone here, and [`KeyPress::normalized`] only ever folds
    /// characters this undoing gets exactly right.
    pub fn as_text(self) -> Option<char> {
        if self
            .mods
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        match self.code {
            KeyCode::Char(c) if self.mods.contains(KeyModifiers::SHIFT) && c.is_lowercase() => {
                c.to_uppercase().next()
            }
            KeyCode::Char(c) => Some(c),
            _ => None,
        }
    }

    /// Whether this key needs the Kitty keyboard protocol to arrive at all.
    ///
    ///
    /// True for `Ctrl+Enter`, `Ctrl+Up`/`Ctrl+Down`, any `Shift+F<n>`, any
    /// `Alt+F<n>`, and any `Ctrl+Shift+<anything>` - the set the design
    /// verified against a real `xterm-256color` and mc 4.8.33. `Ctrl+H` is
    /// **not** in it: it arrives, it is simply indistinguishable from
    /// `Backspace`, which the design handles separately and which
    /// `crate::input::resolve_ctrl_h` already implements.
    ///
    /// This is what the page reads to mark a binding as unavailable
    /// and to show the fallback beside it.
    pub const fn needs_enhanced_protocol(&self) -> bool {
        let ctrl = self.mods.contains(KeyModifiers::CONTROL);
        let alt = self.mods.contains(KeyModifiers::ALT);
        let shift = self.mods.contains(KeyModifiers::SHIFT);
        // `Ctrl+Shift+<anything>` first, because it covers `Ctrl+Shift+D`,
        // `Ctrl+Shift+0` and the `Ctrl+Shift+<n>` secondary sorts in one line
        // rather than three.
        if ctrl && shift {
            return true;
        }
        match self.code {
            // A function key with `Shift` or `Alt` is `CSI ...;2~` / `;3~`,
            // which the design verified most TUI applications never see.
            KeyCode::F(_) => shift || alt,
            KeyCode::Enter => ctrl,
            KeyCode::Up | KeyCode::Down => ctrl,
            _ => false,
        }
    }
}

/// The uppercase letter whose fold is `c`, when there is exactly one.
///
/// The inverse of [`fold_case`], and the test for "is `SHIFT` meaningful on
/// this character". `None` for anything that is not the folded half of a pair:
/// `!` and `1` upper-case to themselves, and `ß` upper-cases to two characters,
/// so neither is a letter `SHIFT` could have produced.
fn unfold_case(c: char) -> Option<char> {
    let mut upper = c.to_uppercase();
    let u = upper.next()?;
    if upper.next().is_some() || u == c {
        return None;
    }
    (fold_case(u) == Some(c)).then_some(u)
}

/// The lowercase form of `c`, when upper-casing it again gives back exactly
/// `c` - the condition under which [`KeyPress::normalized`]'s fold can be
/// undone by [`KeyPress::as_text`].
///
/// `None` for the handful of characters whose case mapping is not one-to-one:
/// `İ` U+0130, `ẞ` U+1E9E, and the compatibility forms `ϴ` U+03F4, `Ω` U+2126,
/// `K` U+212A, `Å` U+212B.
fn fold_case(c: char) -> Option<char> {
    let mut lower = c.to_lowercase();
    let folded = lower.next()?;
    if lower.next().is_some() {
        return None;
    }
    let mut upper = folded.to_uppercase();
    let back = upper.next()?;
    if upper.next().is_some() || back != c {
        return None;
    }
    Some(folded)
}

impl From<KeyEvent> for KeyPress {
    fn from(ev: KeyEvent) -> Self {
        Self::new(ev.code, ev.modifiers).normalized()
    }
}

impl fmt::Display for KeyPress {
    /// The inverse of [`parse_key`], so the `F1` reference prints what a user
    /// would have to type into `keymap.toml`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.mods.contains(KeyModifiers::CONTROL) {
            f.write_str("ctrl+")?;
        }
        if self.mods.contains(KeyModifiers::ALT) {
            f.write_str("alt+")?;
        }
        if self.mods.contains(KeyModifiers::SHIFT) {
            f.write_str("shift+")?;
        }
        match self.code {
            KeyCode::Char(c) => match c {
                ' ' => f.write_str("space"),
                '+' => f.write_str("plus"),
                '-' => f.write_str("minus"),
                '*' => f.write_str("asterisk"),
                '/' => f.write_str("slash"),
                '\\' => f.write_str("backslash"),
                '.' => f.write_str("period"),
                ',' => f.write_str("comma"),
                other => write!(f, "{other}"),
            },
            KeyCode::F(n) => write!(f, "f{n}"),
            KeyCode::Enter => f.write_str("enter"),
            KeyCode::Esc => f.write_str("esc"),
            KeyCode::Tab => f.write_str("tab"),
            KeyCode::BackTab => f.write_str("backtab"),
            KeyCode::Backspace => f.write_str("backspace"),
            KeyCode::Delete => f.write_str("delete"),
            KeyCode::Insert => f.write_str("insert"),
            KeyCode::Home => f.write_str("home"),
            KeyCode::End => f.write_str("end"),
            KeyCode::PageUp => f.write_str("pgup"),
            KeyCode::PageDown => f.write_str("pgdn"),
            KeyCode::Up => f.write_str("up"),
            KeyCode::Down => f.write_str("down"),
            KeyCode::Left => f.write_str("left"),
            KeyCode::Right => f.write_str("right"),
            other => write!(f, "{other:?}"),
        }
    }
}

/// A binding as written in `keymap.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Binding {
    /// A single key press.
    Key(KeyPress),
    /// A two-key chord: press the first, then the second.
    Chord(KeyPress, KeyPress),
}

impl Binding {
    /// The key that starts this binding.
    pub const fn first(&self) -> KeyPress {
        match self {
            Self::Key(k) | Self::Chord(k, _) => *k,
        }
    }

    /// Parse `"ctrl+x l"` or `"shift+f10"`.
    pub fn parse(text: &str) -> Result<Self> {
        let mut parts = text.split_whitespace();
        let first = parts.next().ok_or_else(|| Error::Binding {
            binding: text.to_string(),
            reason: "empty binding".to_string(),
        })?;
        let first = parse_key(first).map_err(|reason| Error::Binding {
            binding: text.to_string(),
            reason,
        })?;
        let Some(second) = parts.next() else {
            return Ok(Self::Key(first));
        };
        let second = parse_key(second).map_err(|reason| Error::Binding {
            binding: text.to_string(),
            reason,
        })?;
        if parts.next().is_some() {
            return Err(Error::Binding {
                binding: text.to_string(),
                reason: "only two-key chords are supported".to_string(),
            });
        }
        Ok(Self::Chord(first, second))
    }
}

impl fmt::Display for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Key(k) => write!(f, "{k}"),
            Self::Chord(a, b) => write!(f, "{a} {b}"),
        }
    }
}

/// Parse one key press: modifiers joined with `+`, then a key name.
///
/// Returns the reason as a `String` so the caller can attach the binding text.
pub fn parse_key(text: &str) -> std::result::Result<KeyPress, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("empty key".to_string());
    }
    let lower = text.to_ascii_lowercase();

    // Split on '+', but a trailing bare '+' is the plus *key*: "ctrl++".
    let mut mods = KeyModifiers::NONE;
    let mut rest = lower.as_str();
    while let Some(idx) = rest.find('+') {
        if idx == 0 {
            // Leading '+' means the key itself.
            break;
        }
        let (head, tail) = rest.split_at(idx);
        let tail = tail.get(1..).unwrap_or("");
        match head {
            "ctrl" | "control" | "c" => mods.insert(KeyModifiers::CONTROL),
            "alt" | "meta" | "m" => mods.insert(KeyModifiers::ALT),
            "shift" | "s" => mods.insert(KeyModifiers::SHIFT),
            "super" | "win" | "cmd" => mods.insert(KeyModifiers::SUPER),
            _ => break,
        }
        rest = tail;
        if rest.is_empty() {
            return Err(format!("{text:?} ends with a modifier"));
        }
    }

    let code = parse_code(rest).ok_or_else(|| format!("unknown key {rest:?} in {text:?}"))?;

    // `shift+n` means the key a terminal reports as `N`. The whole binding was
    // lowercased above so key *names* are case-insensitive, which also flattens
    // a letter's case - and normalisation then clears `SHIFT` from a lowercase
    // `Char` (see the module docs), so without this `shift+n` and `n` would be
    // the same binding and `Shift+N` would be unspellable. Re-apply the case the
    // modifier asked for; punctuation and digits keep the existing rule, where
    // *which* character arrived already carries the shifted state.
    let code = match code {
        KeyCode::Char(c) if mods.contains(KeyModifiers::SHIFT) && c.is_ascii_lowercase() => {
            KeyCode::Char(c.to_ascii_uppercase())
        }
        other => other,
    };
    Ok(KeyPress::new(code, mods).normalized())
}

/// Parse the key-name half of a binding.
fn parse_code(name: &str) -> Option<KeyCode> {
    // Function keys, f1..f35 (the enhanced protocol reports up to f35).
    if let Some(digits) = name.strip_prefix('f')
        && !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit())
        && let Ok(n) = digits.parse::<u8>()
        && (1..=35).contains(&n)
    {
        return Some(KeyCode::F(n));
    }

    let code = match name {
        "enter" | "return" | "cr" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pgup" | "pageup" | "page_up" => KeyCode::PageUp,
        "pgdn" | "pagedown" | "page_down" => KeyCode::PageDown,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "space" => KeyCode::Char(' '),
        "plus" => KeyCode::Char('+'),
        "minus" | "dash" | "hyphen" => KeyCode::Char('-'),
        "asterisk" | "star" => KeyCode::Char('*'),
        "slash" => KeyCode::Char('/'),
        "backslash" => KeyCode::Char('\\'),
        "period" | "dot" | "full_stop" => KeyCode::Char('.'),
        "comma" => KeyCode::Char(','),
        "semicolon" => KeyCode::Char(';'),
        "colon" => KeyCode::Char(':'),
        "equal" | "equals" => KeyCode::Char('='),
        "underscore" => KeyCode::Char('_'),
        "tilde" => KeyCode::Char('~'),
        "backtick" | "grave" => KeyCode::Char('`'),
        "quote" | "apostrophe" => KeyCode::Char('\''),
        "lbracket" => KeyCode::Char('['),
        "rbracket" => KeyCode::Char(']'),
        "menu" => KeyCode::Menu,
        _ => {
            let mut chars = name.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            KeyCode::Char(c)
        }
    };
    Some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(text: &str) -> KeyPress {
        parse_key(text).unwrap_or_else(|e| panic!("{text}: {e}"))
    }

    #[test]
    fn normalising_twice_is_normalising_once() {
        // The property that makes the whole comparison scheme safe, and the one
        // whose absence was invisible: `From<KeyEvent>` normalises, and
        // `Keymap::resolve` normalises what it is given *again*. Anything that
        // changed on the second pass was a binding that could never resolve -
        // which is what happened to the `Shift+N`.
        let cases = [
            KeyPress::new(KeyCode::Char('N'), KeyModifiers::NONE),
            KeyPress::new(KeyCode::Char('N'), KeyModifiers::SHIFT),
            KeyPress::new(KeyCode::Char('n'), KeyModifiers::SHIFT),
            KeyPress::new(KeyCode::Char('!'), KeyModifiers::SHIFT),
            KeyPress::new(KeyCode::Char('1'), KeyModifiers::SHIFT),
            KeyPress::new(
                KeyCode::Char('3'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            KeyPress::new(KeyCode::Char('\u{1e9e}'), KeyModifiers::SHIFT),
            KeyPress::new(KeyCode::BackTab, KeyModifiers::NONE),
            KeyPress::new(KeyCode::F(3), KeyModifiers::SHIFT),
        ];
        for press in cases {
            let once = press.normalized();
            assert_eq!(once, once.normalized(), "{press:?} is not stable");
        }
    }

    #[test]
    fn shift_is_kept_on_a_letter_and_dropped_on_what_shift_already_typed() {
        // `shift+n` has to stay distinct from `n`, or there is no `Shift+N`.
        assert_eq!(
            KeyPress::new(KeyCode::Char('N'), KeyModifiers::NONE).normalized(),
            key("shift+n"),
        );
        assert_ne!(
            KeyPress::new(KeyCode::Char('N'), KeyModifiers::NONE).normalized(),
            key("n"),
        );
        // `!` is what `shift+1` types, so the flag would only make a second,
        // unreachable spelling of it.
        assert_eq!(
            KeyPress::new(KeyCode::Char('!'), KeyModifiers::SHIFT).normalized(),
            key("!"),
        );
        // And the case a capital still types itself: `as_text` must give the
        // character back, whichever way the terminal reported it.
        assert_eq!(
            KeyPress::new(KeyCode::Char('N'), KeyModifiers::NONE)
                .normalized()
                .as_text(),
            Some('N'),
        );
    }

    #[test]
    fn every_binding_form_named_in_the_task_parses() {
        assert_eq!(
            key("ctrl+enter"),
            KeyPress::new(KeyCode::Enter, KeyModifiers::CONTROL)
        );
        assert_eq!(
            key("alt+f1"),
            KeyPress::new(KeyCode::F(1), KeyModifiers::ALT)
        );
        assert_eq!(
            key("shift+f10"),
            KeyPress::new(KeyCode::F(10), KeyModifiers::SHIFT)
        );
        assert_eq!(
            key("ctrl+alt+r"),
            KeyPress::new(
                KeyCode::Char('r'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            )
        );
        assert_eq!(
            key("alt+period"),
            KeyPress::new(KeyCode::Char('.'), KeyModifiers::ALT)
        );
        assert_eq!(
            key("ctrl+backslash"),
            KeyPress::new(KeyCode::Char('\\'), KeyModifiers::CONTROL)
        );
        assert_eq!(key("plus"), KeyPress::plain(KeyCode::Char('+')));
        assert_eq!(key("minus"), KeyPress::plain(KeyCode::Char('-')));
        assert_eq!(key("asterisk"), KeyPress::plain(KeyCode::Char('*')));
        assert_eq!(key("slash"), KeyPress::plain(KeyCode::Char('/')));
        assert_eq!(key("backspace"), KeyPress::plain(KeyCode::Backspace));
        assert_eq!(key("pgup"), KeyPress::plain(KeyCode::PageUp));
        assert_eq!(key("pgdn"), KeyPress::plain(KeyCode::PageDown));
        assert_eq!(key("esc"), KeyPress::plain(KeyCode::Esc));
        assert_eq!(key("tab"), KeyPress::plain(KeyCode::Tab));
        for n in 1..=12u8 {
            assert_eq!(key(&format!("f{n}")), KeyPress::plain(KeyCode::F(n)));
        }
        for n in 1..=9u32 {
            let c = char::from_digit(n, 10).unwrap_or('0');
            assert_eq!(
                key(&format!("alt+{n}")),
                KeyPress::new(KeyCode::Char(c), KeyModifiers::ALT)
            );
            assert_eq!(
                key(&format!("ctrl+{n}")),
                KeyPress::new(KeyCode::Char(c), KeyModifiers::CONTROL)
            );
        }
    }

    #[test]
    fn chords_parse_and_print() {
        let b = Binding::parse("ctrl+x l").expect("chord");
        assert_eq!(
            b,
            Binding::Chord(
                KeyPress::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
                KeyPress::plain(KeyCode::Char('l')),
            )
        );
        assert_eq!(b.to_string(), "ctrl+x l");
    }

    #[test]
    fn normalisation_makes_shifted_punctuation_and_case_agree() {
        // The same physical key, reported two ways by two terminals.
        let with_shift = KeyPress::new(KeyCode::Char('+'), KeyModifiers::SHIFT).normalized();
        let without = KeyPress::plain(KeyCode::Char('+')).normalized();
        assert_eq!(with_shift, without);
        assert_eq!(with_shift, key("plus"));

        // Uppercase folds to lowercase + SHIFT.
        assert_eq!(
            KeyPress::plain(KeyCode::Char('A')).normalized(),
            KeyPress::new(KeyCode::Char('a'), KeyModifiers::SHIFT)
        );
        // BackTab is spelled shift+tab.
        assert_eq!(
            KeyPress::plain(KeyCode::BackTab).normalized(),
            key("shift+tab")
        );
    }

    #[test]
    fn bad_bindings_are_errors_not_panics() {
        assert!(Binding::parse("").is_err());
        assert!(Binding::parse("ctrl+").is_err());
        assert!(Binding::parse("ctrl+nonsense").is_err());
        assert!(Binding::parse("a b c").is_err());
        assert!(Binding::parse("f99").is_err());
    }

    #[test]
    fn display_round_trips_through_the_parser() {
        for text in [
            "ctrl+enter",
            "alt+f1",
            "shift+f10",
            "ctrl+alt+r",
            "alt+period",
            "ctrl+backslash",
            "plus",
            "esc",
            "pgdn",
        ] {
            let k = key(text);
            assert_eq!(key(&k.to_string()), k, "round trip for {text}");
        }
    }

    #[test]
    fn a_character_whose_case_does_not_round_trip_is_typed_as_itself() {
        // the character that arrived is the character
        // that goes into the quick-search buffer and into the command line.
        // Folding `İ` to lowercase and back yields `I`, and `ẞ` yields `S`,
        // so neither is folded at all.
        for c in [
            '\u{130}', '\u{1E9E}', '\u{3F4}', '\u{2126}', '\u{212A}', '\u{212B}',
        ] {
            let press = KeyPress::plain(KeyCode::Char(c)).normalized();
            assert_eq!(press.code, KeyCode::Char(c), "{c:?} was folded");
            assert_eq!(press.as_text(), Some(c), "{c:?} types as something else");
            // ...and the same key arriving with SHIFT set, as the enhanced
            // protocol may report it.
            let shifted = KeyPress::new(KeyCode::Char(c), KeyModifiers::SHIFT).normalized();
            assert_eq!(shifted, press, "{c:?} with and without SHIFT must agree");
            assert_eq!(shifted.as_text(), Some(c));
        }
    }

    #[test]
    fn every_folded_character_survives_the_round_trip() {
        // Whatever `normalized` does fold, `as_text` has to undo exactly.
        for c in ('\u{0}'..='\u{2FFF}').filter(|c| c.is_uppercase()) {
            let typed = KeyPress::plain(KeyCode::Char(c)).normalized().as_text();
            assert_eq!(typed, Some(c), "{c:?} (U+{:04X}) was replaced", c as u32);
        }
    }

    #[test]
    fn control_and_alt_never_produce_text() {
        assert_eq!(key("ctrl+k").as_text(), None);
        assert_eq!(key("alt+1").as_text(), None);
        assert_eq!(KeyPress::plain(KeyCode::Char('x')).as_text(), Some('x'));
    }

    #[test]
    fn text_keeps_its_case_through_normalisation() {
        // Terminals report an uppercase letter as Char('T') + SHIFT; crossterm
        // adds the SHIFT itself. Normalisation folds it to ('t', SHIFT) for
        // binding lookup, and as_text has to hand back the 'T' that was typed
        // or smart-case quick search can never engage.
        let shifted = KeyPress::new(KeyCode::Char('T'), KeyModifiers::SHIFT).normalized();
        assert_eq!(
            shifted,
            KeyPress::new(KeyCode::Char('t'), KeyModifiers::SHIFT)
        );
        assert_eq!(shifted.as_text(), Some('T'));
        assert_eq!(
            KeyPress::plain(KeyCode::Char('T')).normalized().as_text(),
            Some('T')
        );
        assert_eq!(KeyPress::plain(KeyCode::Char('t')).as_text(), Some('t'));
        // Shifted punctuation already arrives as the shifted character.
        assert_eq!(key("plus").as_text(), Some('+'));
    }
}
