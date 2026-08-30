//! `OSC 52`, the terminal's own clipboard sequence.
//!
//! > **`OSC 52`**, the terminal's own clipboard sequence. It is in-band, so it
//! > works over SSH without an agent, a forwarded socket, or a clipboard daemon
//! > on the remote host - the same reason the design chooses an in-band keyboard
//! > protocol. And it needs no dependency, which the alternatives all do.
//!
//! This module is the **only** place in the crate that spells an escape
//! sequence for the clipboard, and [`base64`] is the only base64 in the tree.
//! Both facts are deliberate: the design chose `OSC 52` partly for needing
//! no dependency, so reaching for a base64 crate to spell it would undo the
//! reason it was chosen. The encoder below is
//! twenty lines and is held to RFC 4648 section 10's own test vectors.
//!
//! # Why the sequence is sent bare
//!
//! No tmux `DCS` wrapper. tmux's default `set-clipboard external` accepts
//! `OSC 52` from an application and forwards it to the outer terminal, so a
//! wrapper would be the thing that broke the default case - it needs
//! `allow-passthrough`, which is off by default. A user who has turned
//! `set-clipboard` off gets the internal clipboard instead, which is the case
//! the fallback exists for. `screen`'s chunked
//! `DCS` form is deliberately not implemented: it is a second wire format for a
//! case the fallback already covers.
//!
//! # Why nothing here reports whether it worked
//!
//! It cannot be known. A terminal that ignores `OSC 52` says nothing, and a
//! terminal that permits *writing* the clipboard commonly refuses to answer a
//! *read* of it - so there is no reply to wait for and no capability to query.
//! [`write`]'s `bool` is the one honest answer available: whether there was a
//! terminal on stdout to write to at all. Everything else is the caller's
//! once-a-session notice ("told about once, not on every copy").

use std::io::{IsTerminal, Write, stdout};

/// The base64 alphabet, RFC 4648 section 4 (the standard one, not URL-safe).
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `ESC ] 52 ; c ; <base64> ESC \`.
///
/// `c` is the selection: the system clipboard, which is the one `Ctrl+V` in
/// another application reads. `ESC \` (`ST`) terminates rather than `BEL`,
/// because `ST` is what the standard says and every terminal that implements
/// the sequence at all accepts it.
pub fn sequence(text: &str) -> String {
    let payload = base64(text.as_bytes());
    let mut out = String::with_capacity(payload.len().saturating_add(16));
    out.push_str("\u{1b}]52;c;");
    out.push_str(&payload);
    out.push_str("\u{1b}\\");
    out
}

/// Standard base64, with padding (RFC 4648 section 4).
///
/// In-tree rather than from a crate: see the module documentation.
pub fn base64(bytes: &[u8]) -> String {
    // Four output characters per three input bytes, rounded up.
    let groups = bytes.len().div_ceil(3);
    let mut out = String::with_capacity(groups.saturating_mul(4));
    // The mask keeps the index inside the table by construction; `get` says so
    // to the compiler as well, because this crate does not index slices.
    let digit = |six: u8| {
        char::from(
            ALPHABET
                .get(usize::from(six & 0x3F))
                .copied()
                .unwrap_or(b'A'),
        )
    };
    for chunk in bytes.chunks(3) {
        // Missing bytes are zero, and the characters they would have produced
        // become `=` below - which is what padding *is*.
        let b0 = chunk.first().copied().unwrap_or(0);
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let word = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(digit((word >> 18) as u8));
        out.push(digit((word >> 12) as u8));
        out.push(if chunk.len() > 1 {
            digit((word >> 6) as u8)
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            digit(word as u8)
        } else {
            '='
        });
    }
    out
}

/// Write the sequence to stdout and flush.
///
/// `Ok(false)` when stdout is not a terminal - a piped run has no clipboard to
/// write to, and the internal clipboard is then the whole of the answer.
/// `Err` only for a real write failure.
///
/// Writing straight to stdout rather than through the ratatui backend is
/// deliberate: this is not a cell on the screen and must not be drawn, cached
/// or diffed. It is a message to the terminal that happens to travel the same
/// wire, exactly as the keyboard-protocol push in [`crate::term`] does.
pub fn write(text: &str) -> std::io::Result<bool> {
    let mut out = stdout();
    if !out.is_terminal() {
        return Ok(false);
    }
    out.write_all(sequence(text).as_bytes())?;
    out.flush()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_4648_section_10() {
        // The vectors the RFC itself lists, which is the whole reason this
        // encoder can be twenty lines in-tree rather than a dependency.
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn every_byte_value_round_trips_through_the_alphabet() {
        // Not a decoder test - there is no decoder - but a check that no input
        // byte can produce a character outside the alphabet, which is what a
        // terminal parsing the sequence relies on.
        let all: Vec<u8> = (0..=255_u8).collect();
        let encoded = base64(&all);
        assert!(
            encoded.bytes().all(|b| ALPHABET.contains(&b) || b == b'='),
            "encoded to something outside the alphabet"
        );
        assert_eq!(encoded.len() % 4, 0, "base64 is written in quartets");
    }

    #[test]
    fn the_sequence_is_bare_and_ends_with_st() {
        let seq = sequence("hi");
        assert_eq!(seq, "\u{1b}]52;c;aGk=\u{1b}\\");
        // No tmux DCS wrapper: see the module documentation for why.
        assert!(!seq.contains("\u{1b}Ptmux"));
    }

    #[test]
    fn a_multi_byte_character_survives_as_its_utf8_bytes() {
        // The clipboard is always text and the payload is the
        // text's UTF-8, not its code points.
        assert_eq!(base64("é".as_bytes()), base64(&[0xC3, 0xA9]));
    }

    #[test]
    fn an_empty_copy_still_produces_a_well_formed_sequence() {
        // Clearing the clipboard is what an empty payload means to a terminal,
        // and it must not be a truncated escape that eats the next keystroke.
        assert_eq!(sequence(""), "\u{1b}]52;c;\u{1b}\\");
    }
}
