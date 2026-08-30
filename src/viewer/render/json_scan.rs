//! The byte scanner the JSON tree is built from.
//!
//! Deliberately not a tokenizer that produces a token stream: the renderer
//! asks for the next thing it needs at the point it needs it, so nothing is
//! allocated per token and the walk carries no state but its position.

/// A scanner over a document's bytes.
pub struct Scan<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Scan<'a> {
    /// Start at the front of `text`.
    pub const fn new(text: &'a str) -> Self {
        Self {
            bytes: text.as_bytes(),
            at: 0,
        }
    }

    /// The byte at the cursor, or `None` at the end.
    pub fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    /// Step over whitespace.
    /// Step over whitespace.
    pub fn skip_space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.at = self.at.saturating_add(1);
        }
    }

    /// Take the byte at the cursor when it is `want`.
    /// Step over the byte at the cursor, whatever it is.
    ///
    /// The caller has already looked at it with [`Scan::peek`]; this is the
    /// half that consumes it.
    pub fn bump(&mut self) {
        self.at = self.at.saturating_add(1);
    }

    /// Take the byte at the cursor when it is `want`.
    pub fn eat(&mut self, want: u8) -> bool {
        if self.peek() == Some(want) {
            self.at = self.at.saturating_add(1);
            return true;
        }
        false
    }

    /// The raw text of a string literal, quotes included, or `None` where the
    /// document ends inside it.
    /// A string literal, quotes included.
    pub fn string(&mut self) -> Option<&'a str> {
        let from = self.at;
        if !self.eat(b'"') {
            return None;
        }
        loop {
            match self.peek()? {
                // An escape takes the byte after it with it, which is what
                // keeps a `\"` from ending the string.
                b'\\' => self.at = self.at.saturating_add(2),
                b'"' => {
                    self.at = self.at.saturating_add(1);
                    return self
                        .bytes
                        .get(from..self.at)
                        .and_then(|b| std::str::from_utf8(b).ok());
                }
                _ => self.at = self.at.saturating_add(1),
            }
        }
    }

    /// The raw text of a number, `true`, `false` or `null`.
    /// A number, `true`, `false` or `null`, as written.
    pub fn literal(&mut self) -> Option<&'a str> {
        let from = self.at;
        while matches!(
            self.peek(),
            Some(b'-' | b'+' | b'.' | b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z')
        ) {
            self.at = self.at.saturating_add(1);
        }
        if self.at == from {
            return None;
        }
        self.bytes
            .get(from..self.at)
            .and_then(|b| std::str::from_utf8(b).ok())
    }
}
