//! Encoding a crossterm key back into the bytes a shell expects.
//!
//!
//! This is the fiddly half of console mode and it lives here **once**. The
//! application decodes what the terminal sent it into a [`KeyPress`],
//! decides whether the key is one of the few it intercepts,
//! and everything else has to be turned back into the byte
//! sequence a terminal would have sent - because the shell on the other end of
//! the PTY is talking to a terminal, and we are it.
//!
//! Three things are worth knowing before reading the table:
//!
//! * **Case is restored.** [`KeyPress::normalized`] folds an uppercase
//!   character to lowercase plus `SHIFT` so that `ctrl+a` and `Ctrl+A` are one
//!   binding. A shell wants the `A`, so the fold is undone here exactly as
//!   [`KeyPress::as_text`] undoes it.
//! * **Application cursor mode matters.** A full-screen program - `less`,
//!   `vim`, anything with readline in vi mode - switches the terminal to
//!   `DECCKM`, after which `Up` is `ESC O A` rather than `ESC [ A`. `vt100`
//!   tracks the mode for us ([`vt100::Screen::application_cursor`]), so
//!   [`TerminalMode`] carries it in and the arrow keys read it.
//! * **Modified specials use the xterm parameter.** `1 + (shift 1 | alt 2 |
//!   ctrl 4)`, in `CSI 1 ; <m> <letter>` for the cursor keys and `CSI <n> ; <m>
//!   ~` for the numbered ones. It is the encoding crossterm itself parses, so a
//!   key that made it in comes back out the same shape.
//!
//! What is deliberately *not* done: the Kitty protocol is never re-emitted.
//! `vt100` does not parse the flag-push sequence, so nothing on the other side
//! can have asked for it, and inventing `CSI … u` for a shell that never
//! requested it would break every ordinary key.

use crossterm::event::{KeyCode, KeyModifiers};

use crate::input::KeyPress;

/// The modes of the emulated terminal that change what a key encodes to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TerminalMode {
    /// `DECCKM`: the cursor keys send `SS3` rather than `CSI`.
    pub application_cursor: bool,
    /// Whether the program on the PTY asked for bracketed paste.
    pub bracketed_paste: bool,
}

impl TerminalMode {
    /// Read the modes out of the parsed screen.
    pub fn from_screen(screen: &vt100::Screen) -> Self {
        Self {
            application_cursor: screen.application_cursor(),
            bracketed_paste: screen.bracketed_paste(),
        }
    }
}

/// The xterm modifier parameter: `1 + (shift 1 | alt 2 | ctrl 4)`.
///
/// Returns `None` when no modifier is held, which is the case that takes the
/// short, unparameterised form of every sequence.
fn modifier_param(mods: KeyModifiers) -> Option<u8> {
    let mut bits = 0_u8;
    if mods.contains(KeyModifiers::SHIFT) {
        bits |= 1;
    }
    if mods.contains(KeyModifiers::ALT) {
        bits |= 2;
    }
    if mods.contains(KeyModifiers::CONTROL) {
        bits |= 4;
    }
    if bits == 0 {
        None
    } else {
        Some(bits.saturating_add(1))
    }
}

/// `CSI 1 ; m <final>`, or `CSI <final>` unmodified - the cursor-key family.
fn csi_letter(final_byte: char, mods: KeyModifiers, application: bool) -> Vec<u8> {
    match modifier_param(mods) {
        Some(m) => format!("\x1b[1;{m}{final_byte}").into_bytes(),
        // Application cursor mode only applies to the *unmodified* form; xterm
        // sends the CSI form as soon as a modifier is in play.
        None if application => format!("\x1bO{final_byte}").into_bytes(),
        None => format!("\x1b[{final_byte}").into_bytes(),
    }
}

/// `CSI <n> ; m ~`, or `CSI <n> ~` unmodified - the numbered keys.
fn csi_tilde(n: u8, mods: KeyModifiers) -> Vec<u8> {
    match modifier_param(mods) {
        Some(m) => format!("\x1b[{n};{m}~").into_bytes(),
        None => format!("\x1b[{n}~").into_bytes(),
    }
}

/// The control byte a `Ctrl+<char>` produces on a real terminal.
///
/// `Ctrl+A`-`Ctrl+Z` are 1-26; the six that follow `Z` in ASCII are the ones a
/// shell actually uses (`Ctrl+[` is `Esc`, `Ctrl+\` quits, `Ctrl+_` is undo in
/// readline). A character with no control form is sent as itself, which is what
/// a terminal does.
fn control_byte(c: char) -> Vec<u8> {
    let byte = match c.to_ascii_lowercase() {
        c @ 'a'..='z' => Some((c as u8).saturating_sub(b'a').saturating_add(1)),
        '@' | ' ' | '2' => Some(0),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '/' | '7' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        '3' => Some(0x1b),
        '4' => Some(0x1c),
        '5' => Some(0x1d),
        _ => None,
    };
    match byte {
        Some(b) => vec![b],
        None => {
            let mut buf = [0_u8; 4];
            c.encode_utf8(&mut buf).as_bytes().to_vec()
        }
    }
}

/// The character a `Char` press would type, with the case fold of
/// [`KeyPress::normalized`] undone.
fn typed_char(c: char, mods: KeyModifiers) -> char {
    if mods.contains(KeyModifiers::SHIFT) && c.is_lowercase() {
        c.to_uppercase().next().unwrap_or(c)
    } else {
        c
    }
}

/// Encode one key press as the bytes a shell expects.
///
/// `None` means the key has no encoding at all - a bare modifier press, a media
/// key, a `KeyCode` this terminal model does not represent - and is dropped
/// rather than sent as something else.
pub fn encode(press: KeyPress, mode: TerminalMode) -> Option<Vec<u8>> {
    let KeyPress { code, mods } = press;
    let alt = mods.contains(KeyModifiers::ALT);
    let ctrl = mods.contains(KeyModifiers::CONTROL);

    let body: Vec<u8> = match code {
        KeyCode::Char(c) => {
            if ctrl {
                control_byte(c)
            } else {
                let c = typed_char(c, mods);
                let mut buf = [0_u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
        }
        // `Ctrl+Enter` is intercepted by the design and never reaches here;
        // any other modified Enter is a plain carriage return, which is what a
        // terminal without the Kitty protocol sends - and nothing on the other
        // side of this PTY asked for that protocol.
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => {
            if mods.contains(KeyModifiers::SHIFT) {
                b"\x1b[Z".to_vec()
            } else {
                vec![b'\t']
            }
        }
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        // DEL, not BS: that is what every terminal emulator sends for the key
        // labelled Backspace, and it is the byte `stty erase` is set to.
        // `Ctrl+Backspace` is BS, which readline binds to backward-kill-word.
        KeyCode::Backspace => {
            if ctrl {
                vec![0x08]
            } else {
                vec![0x7f]
            }
        }
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => csi_letter('A', mods, mode.application_cursor),
        KeyCode::Down => csi_letter('B', mods, mode.application_cursor),
        KeyCode::Right => csi_letter('C', mods, mode.application_cursor),
        KeyCode::Left => csi_letter('D', mods, mode.application_cursor),
        KeyCode::Home => csi_letter('H', mods, mode.application_cursor),
        KeyCode::End => csi_letter('F', mods, mode.application_cursor),
        KeyCode::Insert => csi_tilde(2, mods),
        KeyCode::Delete => csi_tilde(3, mods),
        KeyCode::PageUp => csi_tilde(5, mods),
        KeyCode::PageDown => csi_tilde(6, mods),
        KeyCode::F(n) => function_key(n, mods)?,
        KeyCode::Null => vec![0],
        // Modifier presses, media keys, `KeyCode::Menu`, the Kitty protocol's
        // keypad variants: nothing a shell reads, so nothing is sent.
        _ => return None,
    };

    // `Alt` is the `ESC` prefix - a "meta" byte, which is how readline reads it
    // - but only where the sequence is not already an escape sequence of its
    // own, whose modifier parameter has said so already.
    if alt && !body.first().is_some_and(|b| *b == 0x1b) {
        let mut out = Vec::with_capacity(body.len().saturating_add(1));
        out.push(0x1b);
        out.extend_from_slice(&body);
        return Some(out);
    }
    Some(body)
}

/// `F1`-`F12`. The first four are `SS3` when unmodified, as every terminfo for
/// an xterm-alike says, and `CSI 1 ; m P`-`S` when they are not.
fn function_key(n: u8, mods: KeyModifiers) -> Option<Vec<u8>> {
    let tilde = |code: u8| Some(csi_tilde(code, mods));
    match n {
        1..=4 => {
            let final_byte = match n {
                1 => 'P',
                2 => 'Q',
                3 => 'R',
                _ => 'S',
            };
            Some(match modifier_param(mods) {
                Some(m) => format!("\x1b[1;{m}{final_byte}").into_bytes(),
                None => format!("\x1bO{final_byte}").into_bytes(),
            })
        }
        5 => tilde(15),
        6 => tilde(17),
        7 => tilde(18),
        8 => tilde(19),
        9 => tilde(20),
        10 => tilde(21),
        11 => tilde(23),
        12 => tilde(24),
        _ => None,
    }
}

/// Wrap pasted text for the program on the PTY (the bracketed paste).
///
/// The application enables bracketed paste on its *own* terminal so a pasted
/// path is text rather than a burst of navigation keys; the shell on the PTY
/// makes the same request of us, and passing the markers on is what lets it
/// tell a paste from typing. A program that did not ask gets the bytes bare.
///
/// The end marker cannot appear inside the payload - that is what would let a
/// paste end itself early - so it is removed rather than escaped, there being
/// no escape for it in the protocol.
pub fn paste(text: &str, mode: TerminalMode) -> Vec<u8> {
    let clean = text.replace("\x1b[201~", "");
    if !mode.bracketed_paste {
        return clean.into_bytes();
    }
    let mut out = Vec::with_capacity(clean.len().saturating_add(12));
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(clean.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: KeyModifiers = KeyModifiers::NONE;
    const CTRL: KeyModifiers = KeyModifiers::CONTROL;
    const ALT: KeyModifiers = KeyModifiers::ALT;
    const SHIFT: KeyModifiers = KeyModifiers::SHIFT;

    fn enc(code: KeyCode, mods: KeyModifiers) -> Vec<u8> {
        encode(KeyPress::new(code, mods), TerminalMode::default()).unwrap_or_default()
    }

    #[test]
    fn plain_characters_are_themselves() {
        assert_eq!(enc(KeyCode::Char('a'), NONE), b"a");
        assert_eq!(enc(KeyCode::Char(' '), NONE), b" ");
        assert_eq!(enc(KeyCode::Char('é'), NONE), "é".as_bytes());
    }

    #[test]
    fn the_case_fold_of_normalisation_is_undone() {
        // KeyPress::normalized turns `A` into lowercase + SHIFT so that
        // `ctrl+a` is one binding. A shell wants the capital letter back.
        assert_eq!(enc(KeyCode::Char('a'), SHIFT), b"A");
    }

    #[test]
    fn control_characters() {
        assert_eq!(enc(KeyCode::Char('c'), CTRL), &[0x03]);
        assert_eq!(enc(KeyCode::Char('d'), CTRL), &[0x04]);
        assert_eq!(enc(KeyCode::Char('u'), CTRL), &[0x15]);
        assert_eq!(enc(KeyCode::Char('w'), CTRL), &[0x17]);
        assert_eq!(enc(KeyCode::Char('r'), CTRL), &[0x12]);
        assert_eq!(enc(KeyCode::Char(' '), CTRL), &[0x00]);
        assert_eq!(enc(KeyCode::Char('['), CTRL), &[0x1b]);
        assert_eq!(enc(KeyCode::Char('_'), CTRL), &[0x1f]);
    }

    #[test]
    fn alt_is_an_escape_prefix() {
        assert_eq!(enc(KeyCode::Char('b'), ALT), b"\x1bb");
        assert_eq!(enc(KeyCode::Char('.'), ALT), b"\x1b.");
        // Already an escape sequence: the modifier parameter says Alt, and a
        // second ESC in front of it would be a different key entirely.
        assert_eq!(enc(KeyCode::Up, ALT), b"\x1b[1;3A");
    }

    #[test]
    fn the_keys_a_shell_line_editor_lives_on() {
        assert_eq!(enc(KeyCode::Enter, NONE), b"\r");
        assert_eq!(enc(KeyCode::Tab, NONE), b"\t");
        assert_eq!(enc(KeyCode::Tab, SHIFT), b"\x1b[Z");
        assert_eq!(enc(KeyCode::BackTab, NONE), b"\x1b[Z");
        assert_eq!(enc(KeyCode::Esc, NONE), b"\x1b");
        // DEL, the byte the key labelled Backspace actually sends.
        assert_eq!(enc(KeyCode::Backspace, NONE), &[0x7f]);
        assert_eq!(enc(KeyCode::Backspace, CTRL), &[0x08]);
    }

    #[test]
    fn cursor_keys_follow_application_mode() {
        let app = TerminalMode {
            application_cursor: true,
            bracketed_paste: false,
        };
        assert_eq!(enc(KeyCode::Up, NONE), b"\x1b[A");
        assert_eq!(
            encode(KeyPress::plain(KeyCode::Up), app).unwrap_or_default(),
            b"\x1bOA"
        );
        // A modifier takes the CSI form even in application mode.
        assert_eq!(
            encode(KeyPress::new(KeyCode::Up, CTRL), app).unwrap_or_default(),
            b"\x1b[1;5A"
        );
        assert_eq!(enc(KeyCode::Home, NONE), b"\x1b[H");
        assert_eq!(enc(KeyCode::End, NONE), b"\x1b[F");
    }

    #[test]
    fn numbered_and_function_keys() {
        assert_eq!(enc(KeyCode::Delete, NONE), b"\x1b[3~");
        assert_eq!(enc(KeyCode::PageUp, NONE), b"\x1b[5~");
        assert_eq!(enc(KeyCode::PageDown, SHIFT), b"\x1b[6;2~");
        assert_eq!(enc(KeyCode::F(1), NONE), b"\x1bOP");
        assert_eq!(enc(KeyCode::F(5), NONE), b"\x1b[15~");
        assert_eq!(enc(KeyCode::F(12), NONE), b"\x1b[24~");
        assert_eq!(enc(KeyCode::F(1), CTRL), b"\x1b[1;5P");
        assert_eq!(
            encode(KeyPress::plain(KeyCode::F(25)), TerminalMode::default()),
            None
        );
    }

    #[test]
    fn keys_with_no_encoding_send_nothing() {
        use crossterm::event::ModifierKeyCode;
        assert_eq!(
            encode(
                KeyPress::plain(KeyCode::Modifier(ModifierKeyCode::LeftShift)),
                TerminalMode::default()
            ),
            None
        );
        assert_eq!(
            encode(KeyPress::plain(KeyCode::Menu), TerminalMode::default()),
            None
        );
    }

    #[test]
    fn a_paste_is_bracketed_only_when_the_program_asked() {
        let bare = TerminalMode::default();
        let bracketed = TerminalMode {
            application_cursor: false,
            bracketed_paste: true,
        };
        assert_eq!(paste("ls -la", bare), b"ls -la");
        assert_eq!(paste("ls -la", bracketed), b"\x1b[200~ls -la\x1b[201~");
        // The end marker cannot be smuggled inside a paste.
        assert_eq!(paste("a\x1b[201~b", bracketed), b"\x1b[200~ab\x1b[201~");
    }
}
