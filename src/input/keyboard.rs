//! What the terminal's keyboard protocol has reported.

use crate::input::{KeyModifiers, KeyPress};

/// What the terminal's keyboard protocol has reported: whether the enhanced
/// protocol is active, which modifiers it says are held, and the one chord
/// prefix that may be half-typed.
///
/// The three belong together because the first decides whether the second can
/// ever be answered. A bare modifier press is not an event a legacy terminal
/// sends at all, so [`Keyboard::mods_held`] stays empty there, which is
/// exactly what the design prescribes: the key bar swaps for the modified
/// labels "where the terminal reports modifier state", and nowhere else.
///
/// The held set does **not** trust the release event to arrive. A terminal may
/// honour only part of the pushed flag set, and a modifier released while the
/// window is unfocused produces no release event anywhere, so every ordinary
/// key event is also taken as a statement about what is held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keyboard {
    /// Whether the Kitty keyboard protocol is active. The `F1`
    /// reference marks unavailable bindings from this.
    pub enhanced: bool,
    /// Which modifiers are being held right now, empty on a terminal that
    /// cannot say.
    pub mods_held: KeyModifiers,
    /// The first half of a chord, waiting for the second.
    pub pending_chord: Option<KeyPress>,
}

impl Keyboard {
    /// Note a modifier the terminal reported pressed or released.
    ///
    /// Only meaningful under the enhanced protocol; a legacy terminal never
    /// calls this, which is what leaves the set empty there.
    pub fn note_modifier(&mut self, bit: KeyModifiers, pressed: bool) {
        if pressed {
            self.mods_held.insert(bit);
        } else {
            self.mods_held.remove(bit);
        }
    }

    /// Take an ordinary key event as a statement about what is held.
    ///
    /// A bit that was pushed and never released clears here, because a key
    /// arriving without it is proof it is not down any more. This is the only
    /// thing that clears a stale bit on a terminal that dropped the release.
    pub fn note_key(&mut self, modifiers: KeyModifiers) {
        self.mods_held &= modifiers;
    }
}

impl Default for Keyboard {
    /// The legacy terminal: no enhanced protocol, so nothing is known to be
    /// held and nothing can be.
    fn default() -> Self {
        Self {
            enhanced: false,
            mods_held: KeyModifiers::NONE,
            pending_chord: None,
        }
    }
}
