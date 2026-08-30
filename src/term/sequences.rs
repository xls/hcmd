//! `[terminal.sequences]` - raw escape sequences bound to logical keys.
//!
//!
//! ```toml
//! [terminal.sequences]
//! "shift+f5" = "[15;2~"
//! ```
//!
//! The table is written *logical key = raw sequence*, which is the direction
//! `examples/config.toml` documents. The `ESC` may be left implicit, as it is
//! there, or written `\e`, `\x1b`, `\033` or `^[`.
//!
//! **What this layer can and cannot reach.** crossterm decodes the terminal's
//! bytes before we see anything, and it is not pluggable: a `CSI` sequence it
//! understands has already become the right `KeyEvent` by the time it reaches
//! us, and one it does not understand it *discards* - `Parser::advance` clears
//! its buffer on a parse error and emits nothing. So this table cannot rescue
//! a sequence crossterm rejects outright; that would need a byte-level reader
//! in place of `crossterm::event::read`.
//!
//! What it does reach is the case that actually bites over ssh: a sequence
//! **split across reads**. crossterm emits a lone `Esc` as soon as a read ends
//! on one, and parses what follows as ordinary characters - so a laggy link
//! turns `ESC [ 1 5 ; 2 ~` into `Esc`, `[`, `1`, `5`, `;`, `2`, `~`, seven
//! events instead of `Shift+F5`. This module puts them back together.
//!
//! The decoder is a strict pass-through when the table is empty - the normal
//! case - so nothing is buffered and no key is ever delayed unless a user has
//! asked for it. With a table configured, a lone `Esc` is held until the next
//! key arrives or the event loop's idle tick calls [`SequenceDecoder::flush`],
//! and anything that turns out not to match is replayed in the order it came.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::input::{KeyPress, parse_key};

/// The longest sequence body we will ever buffer, as a guard against a
/// pathological config entry holding keys hostage.
const MAX_SEQUENCE: usize = 32;

/// A parsed `[terminal.sequences]` table.
///
/// Each entry is *(sequence body, logical key)*, where the body is everything
/// after the leading `ESC`.
#[derive(Debug, Clone, Default)]
pub struct SequenceMap {
    entries: Vec<(Vec<char>, KeyPress)>,
}

impl SequenceMap {
    /// Parse the config table. Never fails: an entry that cannot be understood
    /// is dropped and a warning is returned for it (the design - a bad value
    /// is a warning, never fatal).
    pub fn parse(table: &HashMap<String, String>) -> (Self, Vec<String>) {
        let mut entries: Vec<(Vec<char>, KeyPress)> = Vec::new();
        let mut warnings = Vec::new();

        // HashMap iteration order is unspecified; sort so warnings and
        // match order are reproducible.
        let mut names: Vec<&String> = table.keys().collect();
        names.sort();

        for name in names {
            let Some(raw) = table.get(name) else { continue };
            let key = match parse_key(name) {
                Ok(key) => key,
                Err(reason) => {
                    warnings.push(format!(
                        "config.toml: [terminal.sequences] {name:?} is not a key: {reason}"
                    ));
                    continue;
                }
            };
            let body = match sequence_body(raw) {
                Ok(body) => body,
                Err(reason) => {
                    warnings.push(format!(
                        "config.toml: [terminal.sequences] {name:?} = {raw:?}: {reason}"
                    ));
                    continue;
                }
            };
            if entries.iter().any(|(existing, _)| *existing == body) {
                warnings.push(format!(
                    "config.toml: [terminal.sequences] {name:?} repeats a sequence already bound; ignored"
                ));
                continue;
            }
            entries.push((body, key));
        }

        (Self { entries }, warnings)
    }

    /// Nothing configured - the decoder can be a pure pass-through.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many sequences are bound.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The key bound to exactly this body, if any.
    fn exact(&self, body: &[char]) -> Option<KeyPress> {
        self.entries
            .iter()
            .find(|(seq, _)| seq.as_slice() == body)
            .map(|(_, key)| *key)
    }

    /// Whether some bound sequence starts with `body` and is longer, so it is
    /// worth waiting for more characters.
    fn extendable(&self, body: &[char]) -> bool {
        self.entries
            .iter()
            .any(|(seq, _)| seq.len() > body.len() && seq.starts_with(body))
    }
}

/// Turn the configured text into the characters that follow `ESC`.
///
/// Accepts the escape spellings people actually write: a literal `ESC` byte,
/// `\e`, `\x1b`, `\033`, `^[`, or nothing at all - `examples/config.toml`
/// writes `"[15;2~"` with the `ESC` left implicit.
fn sequence_body(raw: &str) -> Result<Vec<char>, String> {
    let unescaped = unescape(raw)?;
    let mut chars: Vec<char> = unescaped.chars().collect();
    if chars.first() == Some(&'\u{1b}') {
        chars.remove(0);
    }
    if chars.is_empty() {
        return Err("empty sequence".to_string());
    }
    if chars.len() > MAX_SEQUENCE {
        return Err(format!("longer than {MAX_SEQUENCE} characters"));
    }
    if chars.contains(&'\u{1b}') {
        return Err("contains a second ESC; one sequence per entry".to_string());
    }
    Ok(chars)
}

/// Expand the backslash escapes a user may reasonably write in TOML.
fn unescape(raw: &str) -> Result<String, String> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();

    // A leading `^[` is the caret notation for ESC.
    if raw.starts_with("^[") {
        out.push('\u{1b}');
        chars.next();
        chars.next();
    }

    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('e') | Some('E') => out.push('\u{1b}'),
            Some('\\') => out.push('\\'),
            Some('x') | Some('X') => {
                let mut digits = String::new();
                while digits.len() < 2 && chars.peek().is_some_and(char::is_ascii_hexdigit) {
                    if let Some(d) = chars.next() {
                        digits.push(d);
                    }
                }
                let value = u32::from_str_radix(&digits, 16)
                    .map_err(|_| "\\x needs one or two hex digits".to_string())?;
                out.push(char::from_u32(value).ok_or("\\x is not a character")?);
            }
            Some(d) if d.is_digit(8) => {
                let mut digits = String::from(d);
                while digits.len() < 3 && chars.peek().is_some_and(|c| c.is_digit(8)) {
                    if let Some(d) = chars.next() {
                        digits.push(d);
                    }
                }
                let value =
                    u32::from_str_radix(&digits, 8).map_err(|_| "bad octal escape".to_string())?;
                out.push(char::from_u32(value).ok_or("octal escape is not a character")?);
            }
            Some(other) => return Err(format!("unknown escape \\{other}")),
            None => return Err("trailing backslash".to_string()),
        }
    }

    Ok(out)
}

/// Rewrites the decoded key stream according to a [`SequenceMap`].
///
/// Feed it every `KeyEvent` that arrives; it hands back the events the
/// application should actually see. With an empty map that is always the event
/// it was given, unchanged and un-delayed.
#[derive(Debug, Clone, Default)]
pub struct SequenceDecoder {
    map: SequenceMap,
    /// The events held back so far: the opening `Esc` and the characters after
    /// it. Kept so a sequence that turns out not to match can be replayed
    /// exactly as it arrived.
    held: Vec<KeyEvent>,
    /// The characters of `held` after the opening `Esc`.
    body: Vec<char>,
}

impl SequenceDecoder {
    /// A decoder for this table.
    pub fn new(map: SequenceMap) -> Self {
        Self {
            map,
            held: Vec::new(),
            body: Vec::new(),
        }
    }

    /// Whether anything is being held back waiting for more characters.
    pub fn is_pending(&self) -> bool {
        !self.held.is_empty()
    }

    /// How many sequences are bound.
    pub fn sequence_count(&self) -> usize {
        self.map.len()
    }

    /// Feed one decoded key event; take back what the application should see.
    pub fn feed(&mut self, event: KeyEvent) -> Vec<KeyEvent> {
        if self.map.is_empty() {
            return vec![event];
        }
        // Releases and repeats never form part of a sequence.
        if event.kind == KeyEventKind::Release {
            return vec![event];
        }

        if self.held.is_empty() {
            if is_bare_escape(&event) {
                self.held.push(event);
                return Vec::new();
            }
            return vec![event];
        }

        // Buffering. Only plain characters extend a sequence.
        let Some(c) = sequence_char(&event) else {
            let mut out = std::mem::take(&mut self.held);
            self.body.clear();
            out.push(event);
            return out;
        };

        self.body.push(c);
        self.held.push(event);

        if let Some(key) = self.map.exact(&self.body) {
            self.held.clear();
            self.body.clear();
            return vec![KeyEvent::new(key.code, key.mods)];
        }
        if self.body.len() < MAX_SEQUENCE && self.map.extendable(&self.body) {
            return Vec::new();
        }
        self.body.clear();
        std::mem::take(&mut self.held)
    }

    /// Give up on a partial sequence - called when no further input arrived.
    ///
    /// Without this a lone `Esc` would be held until the next keystroke.
    pub fn flush(&mut self) -> Vec<KeyEvent> {
        self.body.clear();
        std::mem::take(&mut self.held)
    }
}

/// `Esc` with nothing held down: the only thing that can open a sequence.
fn is_bare_escape(event: &KeyEvent) -> bool {
    event.code == KeyCode::Esc && event.modifiers.is_empty()
}

/// The character this event contributes to a sequence body, if any.
fn sequence_char(event: &KeyEvent) -> Option<char> {
    let interesting = event.modifiers - KeyModifiers::SHIFT;
    if !interesting.is_empty() {
        return None;
    }
    match event.code {
        KeyCode::Char(c) => Some(c),
        _ => None,
    }
}

/// A plain key press, for replaying a sequence that did not match.
#[cfg(test)]
fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_of(pairs: &[(&str, &str)]) -> SequenceMap {
        let table: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        let (map, warnings) = SequenceMap::parse(&table);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        map
    }

    #[test]
    fn the_escape_is_implicit_as_the_example_file_writes_it() {
        let body = sequence_body("[15;2~").unwrap();
        assert_eq!(body, vec!['[', '1', '5', ';', '2', '~']);
    }

    #[test]
    fn every_spelling_of_escape_is_accepted() {
        for raw in ["\\e[15;2~", "\\x1b[15;2~", "\\033[15;2~", "^[[15;2~"] {
            assert_eq!(
                sequence_body(raw).unwrap(),
                vec!['[', '1', '5', ';', '2', '~'],
                "failed on {raw:?}"
            );
        }
    }

    #[test]
    fn a_bad_entry_is_a_warning_and_the_good_ones_still_load() {
        let table: HashMap<String, String> = [
            ("shift+f5".to_string(), "[15;2~".to_string()),
            ("not-a-key".to_string(), "[1~".to_string()),
            ("f6".to_string(), String::new()),
        ]
        .into_iter()
        .collect();
        let (map, warnings) = SequenceMap::parse(&table);
        assert_eq!(map.len(), 1);
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn an_empty_table_is_a_pass_through_with_no_buffering() {
        let mut decoder = SequenceDecoder::default();
        let out = decoder.feed(press(KeyCode::Esc));
        assert_eq!(out.len(), 1);
        assert!(!decoder.is_pending());
    }

    #[test]
    fn a_configured_sequence_becomes_its_logical_key() {
        let mut decoder = SequenceDecoder::new(map_of(&[("shift+f5", "[15;2~")]));
        let mut out = Vec::new();
        out.extend(decoder.feed(press(KeyCode::Esc)));
        for c in "[15;2".chars() {
            out.extend(decoder.feed(press(KeyCode::Char(c))));
        }
        assert!(out.is_empty(), "nothing should escape mid-sequence");
        out.extend(decoder.feed(press(KeyCode::Char('~'))));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].code, KeyCode::F(5));
        assert!(out[0].modifiers.contains(KeyModifiers::SHIFT));
        assert!(!decoder.is_pending());
    }

    #[test]
    fn a_sequence_that_does_not_match_is_replayed_verbatim() {
        let mut decoder = SequenceDecoder::new(map_of(&[("shift+f5", "[15;2~")]));
        let mut out = Vec::new();
        out.extend(decoder.feed(press(KeyCode::Esc)));
        out.extend(decoder.feed(press(KeyCode::Char('['))));
        out.extend(decoder.feed(press(KeyCode::Char('9'))));
        assert_eq!(
            out.iter().map(|e| e.code).collect::<Vec<_>>(),
            vec![KeyCode::Esc, KeyCode::Char('['), KeyCode::Char('9')]
        );
    }

    #[test]
    fn a_non_character_ends_a_partial_sequence_without_losing_it() {
        let mut decoder = SequenceDecoder::new(map_of(&[("shift+f5", "[15;2~")]));
        let _ = decoder.feed(press(KeyCode::Esc));
        let out = decoder.feed(press(KeyCode::Down));
        assert_eq!(
            out.iter().map(|e| e.code).collect::<Vec<_>>(),
            vec![KeyCode::Esc, KeyCode::Down]
        );
    }

    #[test]
    fn a_lone_escape_survives_a_flush() {
        let mut decoder = SequenceDecoder::new(map_of(&[("shift+f5", "[15;2~")]));
        assert!(decoder.feed(press(KeyCode::Esc)).is_empty());
        let out = decoder.flush();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].code, KeyCode::Esc);
        assert!(decoder.flush().is_empty());
    }
}
