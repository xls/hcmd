//! `Alt`+letter accelerators inside a dialog.
//!
//! the design is four sentences of behaviour and one rule about duplicates:
//!
//! > **`Alt` with a letter jumps straight to a control.** The letter is shown
//! > underlined in that control's label [...] **An accelerator never turns
//! > anything off.** [...] Mnemonics are **unique within a dialog**.
//!
//! All of it lives here, once, rather than fourteen times in fourteen
//! `handle_key`s. A dialog declares a table of `(Control, char)` beside its
//! control enum, says what kind of control each one is, and gets the
//! semantics from [`Accelerated::mnemonic_key`] without writing any of them
//! down.
//!
//! # Why a table and not a marker character in the label
//!
//! a file manager draws filenames through the
//! same helpers it draws labels through, and `&` is a legal character in every
//! filesystem this program supports. A shared helper that strips `&` from
//! whatever it is handed will eventually eat a character out of a name. A
//! separate field cannot corrupt a string it does not appear in.
//!
//! The drift a marker scheme would have prevented - a label and its underline
//! disagreeing - is closed by a test per dialog instead, which renders the
//! dialog and reads the underlines off the buffer.
//!

use super::{DialogKey, DialogOutcome};

/// What `Alt`+letter does once it has found its control.
///
/// One value per sentence of the "the rest follows from that", plus the two
/// cases the design does not name and the design settle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accel<C> {
    /// A field, list, combo, radio or tri-state: focus it, change nothing.
    ///
    /// the design gives no meaning to "accelerate a three-way control", and
    /// both candidate meanings - step it, or select the option the letter
    /// names - turn something off. `Space`, `Left` and `Right` are one
    /// keystroke away once the focus is on it.
    ///
    Focus,
    /// A checkbox that gates nothing: focus it and switch it **on**.
    Check,
    /// A checkbox that gates something: switch it **on** and put the caret in
    /// the control it gates. the `Alt+T`.
    Gate(C),
    /// A button: focus it and press it.
    Press,
    /// The control is not on the screen right now - another tab, a collapsed
    /// section, a button this instance does not offer. The key is swallowed
    /// and nothing happens (a dialog consumes all input).
    Absent,
}

/// The letter a table gives `control`.
///
/// `None` for a control with no letter, of which there are ten in the tree and
/// every one has a reason.
pub fn letter_of<C: Copy + PartialEq>(table: &[(C, char)], control: C) -> Option<char> {
    table
        .iter()
        .find(|(candidate, _)| *candidate == control)
        .map(|(_, letter)| *letter)
}

/// The control a table gives `letter`, which is already folded to lower case.
///
/// The comparison ignores ASCII case anyway. [`DialogKey::mnemonic`] folds
/// before it gets here and every table in the tree is written in lower case,
/// so this can only matter for a table that is written the other way - and
/// there the alternative is a letter that is underlined on the screen and
/// silently does nothing, which is exactly the failure the design's
/// uniqueness rule exists to prevent.
pub fn control_for<C: Copy + PartialEq>(table: &[(C, char)], letter: char) -> Option<C> {
    table
        .iter()
        .find(|(_, candidate)| candidate.eq_ignore_ascii_case(&letter))
        .map(|(control, _)| *control)
}

/// The first letter a table declares twice, for the uniqueness test
/// ("a duplicate is a bug rather than a first-one-wins rule").
///
/// > A duplicate is a bug rather than a first-one-wins rule, because the
/// > second control becomes unreachable silently.
///
/// [`control_for`] is what makes it first-one-wins at runtime; this is what
/// makes that unreachable in practice, over the census of every dialog and
/// every tab. Case-insensitive, for the same
/// reason [`control_for`] is: `T` and `t` are one keystroke.
pub fn duplicate_letter<C: Copy>(table: &[(C, char)]) -> Option<char> {
    for (seen, (_, letter)) in table.iter().enumerate() {
        if table
            .iter()
            .take(seen)
            .any(|(_, earlier)| earlier.eq_ignore_ascii_case(letter))
        {
            return Some(*letter);
        }
    }
    None
}

/// A dialog that offers the `Alt` mnemonics.
///
/// Six items to implement and two provided methods, one of which is the whole
/// of the behaviour - so no dialog can spell "never turns anything off"
/// differently from the one beside it.
///
/// It is a separate trait from [`super::Dialog`] because half the things that
/// implement it are not whole dialogs' worth of state and because
/// [`Accelerated::Control`] has to be an associated type: `Dialog` is used as
/// `Box<dyn Dialog>` and an associated type would make it not object safe.
/// [`super::Dialog::mnemonic_letters`] is the object-safe half, and it is what
/// the census reads.
pub trait Accelerated {
    /// How this dialog names a control. An enum wherever there are five or
    /// more of them, so [`Accelerated::accel`] is an exhaustive `match`;
    /// `usize` is allowed for the four-control dialogs that already index
    /// their ring.
    ///
    /// `'static` is forced by [`Accelerated::mnemonics`] returning a `const`
    /// table by `&'static` reference, and is free: every control type in the
    /// tree is a fieldless enum or a `usize`.
    type Control: Copy + PartialEq + 'static;

    /// The letters **currently on the screen**. A tabbed dialog returns the
    /// open tab's table and nothing else, which
    /// is "unique within a dialog" read as "unique among what the user can see
    /// and reach without first pressing `Alt`+digit".
    fn mnemonics(&self) -> &'static [(Self::Control, char)];

    /// What kind of control this is, right now.
    ///
    /// "Right now" because a control can be [`Accel::Absent`] on one instance
    /// of a dialog and live on the next: `Retry failures` on a clean summary,
    /// `Append` on a directory conflict.
    fn accel(&self, control: Self::Control) -> Accel<Self::Control>;

    /// Move focus to a control. A no-op for a control that is not in the ring.
    fn focus_control(&mut self, control: Self::Control);

    /// Switch a checkbox **on**. Never a toggle: the rule lives
    /// here, and a control that is not a checkbox does nothing.
    ///
    /// > A key that enabled on the way in and disabled on the way back would
    /// > make a repeated keystroke destructive, and the user is reaching for
    /// > it because they want to type there.
    fn switch_on(&mut self, control: Self::Control);

    /// Press a button. Only ever called for [`Accel::Press`].
    fn press(&mut self, control: Self::Control) -> DialogOutcome;

    /// The letter drawn underlined in `control`'s label, for the renderer.
    ///
    /// The renderer reads the same table the key handler does, so a label and
    /// the key that reaches it cannot drift apart.
    fn mnemonic_of(&self, control: Self::Control) -> Option<char> {
        letter_of(self.mnemonics(), control)
    }

    /// once, for every dialog.
    ///
    /// `None` when the key is not an `Alt`+letter this dialog claims, which is
    /// the caller's signal to carry on with the rest of `handle_key`. It is
    /// called **before** anything that reads `key.action`, so a global `Alt`
    /// binding of the design cannot pre-empt a mnemonic.
    ///
    fn mnemonic_key(&mut self, key: &DialogKey) -> Option<DialogOutcome> {
        let letter = key.mnemonic()?;
        let control = control_for(self.mnemonics(), letter)?;
        Some(match self.accel(control) {
            Accel::Absent => DialogOutcome::Consumed,
            Accel::Focus => {
                self.focus_control(control);
                DialogOutcome::Consumed
            }
            Accel::Check => {
                self.focus_control(control);
                self.switch_on(control);
                DialogOutcome::Consumed
            }
            // On before focus: a gate that is off may be what makes the gated
            // control unfocusable, and the design wants the caret to land in
            // the field.
            Accel::Gate(gated) => {
                self.switch_on(control);
                self.focus_control(gated);
                DialogOutcome::Consumed
            }
            // Focus first, so a button that acts on "whatever has focus" sees
            // itself, and so a button that pushes a dialog leaves the focus
            // somewhere sensible for when that dialog closes.
            //
            Accel::Press => {
                self.focus_control(control);
                self.press(control)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialog::DialogResult;
    use crate::input::{KeyCode, KeyModifiers, KeyPress};

    /// The controls of the probe dialog below.
    ///
    /// Shaped like the gated half of the Find Files dialog,
    /// because that is the shape the design argues from: a checkbox, the
    /// field it gates, a checkbox that gates nothing, a button, and a control
    /// this instance does not offer.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Control {
        /// A plain field.
        Name,
        /// The gating checkbox, the `Find text`.
        FindText,
        /// The field `FindText` gates. It has no letter of its own: it shares
        /// `Alt+T` with the box that gates it.
        Text,
        /// A checkbox that gates nothing.
        CaseSensitive,
        /// A button.
        Ok,
        /// A button this instance does not offer.
        Retry,
    }

    /// The probe's letters. `t` is the gate's and the gated field has none.
    const MNEMONICS: &[(Control, char)] = &[
        (Control::Name, 's'),
        (Control::FindText, 't'),
        (Control::CaseSensitive, 'c'),
        (Control::Ok, 'o'),
        (Control::Retry, 'r'),
    ];

    /// A dialog with one of every kind of control, and no rendering.
    struct Probe {
        focus: Control,
        find_text: bool,
        case_sensitive: bool,
        text: String,
        /// Every press, with the focus as it stood when `press` was called, so
        /// the "focus moves first" is observable.
        pressed: Vec<(Control, Control)>,
    }

    impl Probe {
        fn new() -> Self {
            Self {
                focus: Control::Name,
                find_text: false,
                case_sensitive: false,
                text: "already typed".to_string(),
                pressed: Vec::new(),
            }
        }
    }

    impl Accelerated for Probe {
        type Control = Control;

        fn mnemonics(&self) -> &'static [(Control, char)] {
            MNEMONICS
        }

        fn accel(&self, control: Control) -> Accel<Control> {
            match control {
                Control::Name | Control::Text => Accel::Focus,
                Control::FindText => Accel::Gate(Control::Text),
                Control::CaseSensitive => Accel::Check,
                Control::Ok => Accel::Press,
                Control::Retry => Accel::Absent,
            }
        }

        fn focus_control(&mut self, control: Control) {
            self.focus = control;
        }

        fn switch_on(&mut self, control: Control) {
            match control {
                Control::FindText => self.find_text = true,
                Control::CaseSensitive => self.case_sensitive = true,
                Control::Name | Control::Text | Control::Ok | Control::Retry => {}
            }
        }

        fn press(&mut self, control: Control) -> DialogOutcome {
            self.pressed.push((control, self.focus));
            DialogOutcome::Accept(DialogResult::None)
        }
    }

    fn alt(letter: char) -> DialogKey {
        DialogKey::raw(KeyPress::new(KeyCode::Char(letter), KeyModifiers::ALT))
    }

    #[test]
    fn a_dialog_mnemonic_is_alt_and_a_letter_and_nothing_else() {
        // "`Alt` with a *letter*, specifically, because the floor is a
        // terminal that sends `Alt+X` as a plain `ESC`-prefixed byte."
        // Both encodings arrive here as the same `KeyPress`, so this is
        // about the modifiers.
        assert_eq!(alt('T').mnemonic(), Some('t'), "folded to lower case");
        assert_eq!(
            DialogKey::raw(KeyPress::new(
                KeyCode::Char('T'),
                KeyModifiers::ALT | KeyModifiers::SHIFT
            ))
            .mnemonic(),
            Some('t'),
            "a terminal that reports the shift is describing the same key"
        );
        assert_eq!(
            DialogKey::raw(KeyPress::new(
                KeyCode::Char('t'),
                KeyModifiers::ALT | KeyModifiers::CONTROL
            ))
            .mnemonic(),
            None,
            "Ctrl+Alt+T is a different binding, not a sloppier spelling"
        );
        assert_eq!(
            DialogKey::raw(KeyPress::plain(KeyCode::Char('t'))).mnemonic(),
            None,
            "a bare letter is text"
        );
        assert_eq!(
            DialogKey::raw(KeyPress::new(KeyCode::F(7), KeyModifiers::ALT)).mnemonic(),
            None
        );
    }

    #[test]
    fn a_mnemonic_is_never_a_digit_so_the_tab_strip_keeps_them() {
        // "`Alt` with a *digit* is already the tab strip, so
        // mnemonics are letters and the two never collide." Half of the
        // guarantee lives here; the other half is `TabStrip::handle`, which
        // claims digits only.
        for digit in '0'..='9' {
            assert_eq!(alt(digit).mnemonic(), None, "Alt+{digit} is the tab strip");
        }
    }

    #[test]
    fn a_gate_ticks_its_box_and_puts_the_caret_in_the_field_it_gates() {
        // "`Alt+T` ticks it *and* puts the caret in the field,
        // which is the one thing the keystroke could sensibly mean."
        let mut probe = Probe::new();
        assert!(matches!(
            probe.mnemonic_key(&alt('t')),
            Some(DialogOutcome::Consumed)
        ));
        assert!(probe.find_text, "the gate is on");
        assert_eq!(probe.focus, Control::Text, "the caret is in the field");
    }

    #[test]
    fn a_mnemonic_never_turns_a_checkbox_off() {
        // the load-bearing sentence: "Pressing it again
        // re-focuses the field and leaves the checkbox alone - a key that
        // enabled on the way in and disabled on the way back would make a
        // repeated keystroke destructive."
        let mut probe = Probe::new();
        for press in 1..=3 {
            probe.mnemonic_key(&alt('t'));
            assert!(probe.find_text, "the gate is still on after press {press}");
            assert_eq!(probe.focus, Control::Text, "and the caret is still there");
        }

        // The same for a checkbox that gates nothing: it ticks and stops.
        // `Space` is how a box is turned off, one keystroke away with the
        // focus now on it.
        for press in 1..=3 {
            probe.mnemonic_key(&alt('c'));
            assert!(probe.case_sensitive, "still on after press {press}");
            assert_eq!(probe.focus, Control::CaseSensitive);
        }
    }

    #[test]
    fn a_field_is_focused_and_nothing_else_changes() {
        // not the text, not the selection. The
        // letter cannot also be typed because `DialogKey::text` is `None`
        // whenever `ALT` is held.
        let mut probe = Probe::new();
        probe.focus = Control::Ok;
        assert!(matches!(
            probe.mnemonic_key(&alt('s')),
            Some(DialogOutcome::Consumed)
        ));
        assert_eq!(probe.focus, Control::Name);
        assert_eq!(probe.text, "already typed");
        assert!(!probe.find_text, "and no box was ticked on the way past");
        assert!(alt('s').text().is_none(), "and the letter does not type");
    }

    #[test]
    fn a_button_takes_focus_before_it_is_pressed() {
        // focus moves first, so a button that
        // acts on "whatever has focus" sees itself.
        let mut probe = Probe::new();
        let outcome = probe.mnemonic_key(&alt('o'));
        assert!(matches!(outcome, Some(DialogOutcome::Accept(_))));
        assert_eq!(
            probe.pressed,
            vec![(Control::Ok, Control::Ok)],
            "focus was already on the button when it was pressed"
        );
    }

    #[test]
    fn an_absent_control_swallows_its_letter() {
        // the key is consumed and nothing
        // happens. Not passed on, because a dialog consumes all input,
        // and it does not open whatever is hiding the control,
        // because that is a second meaning the user did not ask for.
        let mut probe = Probe::new();
        assert!(matches!(
            probe.mnemonic_key(&alt('r')),
            Some(DialogOutcome::Consumed)
        ));
        assert_eq!(probe.focus, Control::Name, "focus did not move");
        assert!(probe.pressed.is_empty(), "and nothing was pressed");
    }

    #[test]
    fn an_unclaimed_letter_falls_through_to_the_rest_of_handle_key() {
        // `None` is the caller's signal to carry on, which is what lets a
        // rebound `alt+x` still reach `is_cancel` in a dialog that does not
        // claim `x`.
        let mut probe = Probe::new();
        assert!(probe.mnemonic_key(&alt('z')).is_none());
        assert!(
            probe
                .mnemonic_key(&DialogKey::raw(KeyPress::plain(KeyCode::Char('t'))))
                .is_none(),
            "and a bare letter is text, not a mnemonic"
        );
        assert_eq!(probe.focus, Control::Name);
    }

    #[test]
    fn a_table_answers_in_both_directions() {
        assert_eq!(letter_of(MNEMONICS, Control::FindText), Some('t'));
        assert_eq!(
            letter_of(MNEMONICS, Control::Text),
            None,
            "the gated field shares the gate's letter and declares none"
        );
        assert_eq!(control_for(MNEMONICS, 't'), Some(Control::FindText));
        assert_eq!(control_for(MNEMONICS, 'z'), None);
        assert_eq!(
            control_for(MNEMONICS, 'T'),
            Some(Control::FindText),
            "`DialogKey::mnemonic` folds, and so does this"
        );
    }

    #[test]
    fn a_duplicate_letter_is_a_bug_the_table_can_be_asked_about() {
        // "A duplicate is a bug rather than a first-one-wins
        // rule, because the second control becomes unreachable silently."
        assert_eq!(duplicate_letter(MNEMONICS), None);
        let clash: &[(Control, char)] = &[
            (Control::Name, 's'),
            (Control::Ok, 'o'),
            (Control::CaseSensitive, 's'),
        ];
        assert_eq!(duplicate_letter(clash), Some('s'));
        let folded: &[(Control, char)] = &[(Control::Name, 's'), (Control::Ok, 'S')];
        assert_eq!(
            duplicate_letter(folded),
            Some('S'),
            "`Alt+S` and `Alt+Shift+S` are one keystroke"
        );
        let empty: &[(Control, char)] = &[];
        assert_eq!(duplicate_letter(empty), None);
    }

    // -----------------------------------------------------------------------
    // The census, and the three invariants that
    // run over it: I2 (unique within a dialog screen), I5 (`Alt`+digit and
    // `Alt`+letter never collide) and I10 (the four reserved letters).
    // -----------------------------------------------------------------------

    /// One dialog screen's mnemonics, for the test the design asks for.
    ///
    /// > Mnemonics are unique within a dialog. A duplicate is a bug rather than
    /// > a first-one-wins rule, because the second control becomes unreachable
    /// > silently; it is caught by a test over every dialog rather than by
    /// > inspection.
    ///
    /// A *screen* and not a dialog, because three dialogs answer differently
    /// depending on what they are showing: the Find dialog per tab, and the
    /// copy, conflict and summary dialogs per what this instance offers.
    struct Screen {
        dialog: &'static str,
        screen: &'static str,
        letters: Vec<char>,
    }

    /// How many `Dialog` implementations the tree has.
    ///
    /// Asserted, so that adding a dialog and forgetting to add it to [`census`]
    /// is a failing test rather than a silent hole. `Dialog::mnemonic_letters`
    /// having no default body is what makes the new dialog declare its letters
    /// at all (I11); this constant is what makes them get tested.
    ///
    /// v0.65 adds three: the connect dialog and its Add-host form,
    /// and the secret prompt.
    ///
    /// v0.7 adds six: the device popup, the menu bar,
    /// the context menu, the execute prompt,
    /// the chooser and the command history.
    ///
    /// The image resizer adds one.
    ///
    const DIALOGS: usize = 24;

    /// How many *screens* those seventeen dialogs present.
    ///
    /// v0.65 adds five: the three above, plus the disconnect
    /// prompt and the unknown-host prompt, which are both
    /// `ConfirmDialog`s with labels of their own.
    ///
    ///
    /// v0.7 adds eight: two for `DrivesDialog` (`Alt+F1`'s devices-and-hotlist
    /// and `Ctrl+D`'s hotlist alone), two for `ContextMenuDialog` (a local
    /// entry and a remote or in-archive one, which differ by the design's
    /// three absent kinds), and one each for `MenuDialog`, `ExecuteDialog`,
    /// `OpenWithDialog` and `HistoryDialog`. `InputDialog` is reused for
    /// `Ctrl+Shift+D`'s label prompt and adds **no** row: its letters are the
    /// same `o`/`n` the existing row already covers.
    ///
    const SCREENS: usize = 35;

    /// the four program-wide reservations.
    const RESERVED_LETTERS: [char; 4] = ['n', 'c', 'h', 'o'];

    /// Every use of a reserved letter in the tree, named (I10).
    ///
    /// `(dialog, screen, letter, the control it names)`. The census is checked
    /// against this in both directions, so a new use of `n`, `c`, `h` or `o`
    /// anywhere in any dialog fails this test until it is written down here.
    const RESERVED_USES: &[(&str, &str, char, &str)] = &[
        ("MenuDialog", "the bar", 'c', "Commands"),
        ("MenuDialog", "the bar", 'n', "Net"),
        ("MenuDialog", "the bar", 'h', "Show"),
        ("MenuDialog", "the bar", 'o', "Configuration"),
        ("ExecuteDialog", "the only one", 'o', "Open with..."),
        ("ExecuteDialog", "the only one", 'n', "Cancel"),
        ("OpenWithDialog", "the only one", 'o', "OK"),
        ("OpenWithDialog", "the only one", 'n', "Cancel"),
        ("ConfirmDialog", "Yes / No", 'n', "No"),
        ("ConfirmDialog", "Delete / Cancel", 'n', "Cancel"),
        ("ConflictDialog", "file over file", 'o', "Overwrite"),
        ("ConflictDialog", "file over file", 'n', "Cancel"),
        (
            "ConflictDialog",
            "directory over directory",
            'o',
            "Overwrite",
        ),
        ("ConflictDialog", "directory over directory", 'n', "Cancel"),
        ("ConflictDialog", "directory over file", 'n', "Cancel"),
        ("CopyMoveDialog", "options collapsed", 'o', "OK"),
        ("CopyMoveDialog", "options collapsed", 'n', "Cancel"),
        ("CopyMoveDialog", "options open", 'c', "On conflict"),
        ("CopyMoveDialog", "options open", 'o', "OK"),
        ("CopyMoveDialog", "options open", 'n', "Cancel"),
        ("FindDialog", "General", 'c', "Case sensitive"),
        (
            "FindDialog",
            "General",
            'o',
            "Find files NOT containing the text",
        ),
        ("FindDialog", "General", 'n', "Cancel"),
        ("FindDialog", "General", 'h', "Help"),
        ("FindDialog", "Advanced", 'n', "Cancel"),
        ("FindDialog", "Advanced", 'h', "Help"),
        ("FindDialog", "Load/Save", 'n', "Cancel"),
        ("FindDialog", "Load/Save", 'h', "Help"),
        ("InputDialog", "the only one", 'o', "OK"),
        ("InputDialog", "the only one", 'n', "Cancel"),
        ("MaskDialog", "the only one", 'o', "OK"),
        ("MaskDialog", "the only one", 'n', "Cancel"),
        ("MultiRenameDialog", "the only one", 'h', "Match case"),
        ("MultiRenameDialog", "the only one", 'c', "Close"),
        ("PackDialog", "the only one", 'c', "Compression"),
        ("PackDialog", "the only one", 'o', "OK"),
        ("PackDialog", "the only one", 'n', "Cancel"),
        ("ResizeDialog", "the only one", 'o', "OK"),
        ("ResizeDialog", "the only one", 'n', "Cancel"),
        ("ProgressDialog", "the only one", 'n', "Cancel"),
        ("RenameDialog", "the only one", 'o', "OK"),
        ("RenameDialog", "the only one", 'n', "Cancel"),
        ("RenameResultDialog", "the only one", 'c', "Close"),
        ("SummaryDialog", "clean", 'c', "Close"),
        ("SummaryDialog", "with failures", 'c', "Close"),
        // the design.
        ("ConfirmDialog", "Disconnect / Cancel", 'n', "Cancel"),
        ("ConfirmDialog", "Accept / Cancel", 'n', "Cancel"),
        ("ConnectDialog", "the only one", 'o', "Connect"),
        ("ConnectDialog", "the only one", 'n', "Cancel"),
        ("HostFormDialog", "the only one", 'o', "OK"),
        ("HostFormDialog", "the only one", 'n', "Cancel"),
        ("SecretDialog", "the only one", 'o', "OK"),
        ("SecretDialog", "the only one", 'n', "Cancel"),
    ];

    /// The uses above that are **not** the letter's program-wide meaning, each
    /// with the reason the design gives for it (I10).
    ///
    /// `c` and `o` are allowed to name something else in a dialog that has no
    /// `Close` and no `OK` respectively - the reservation is a floor on what a
    /// letter may mean, not a requirement that every dialog spend it. `h` has
    /// no such allowance, so multi-rename's is a genuine exception and says so.
    const RESERVED_EXCEPTIONS: &[(&str, &str, char, &str)] = &[
        (
            "MenuDialog",
            "the bar",
            'c',
            "the design names the menu `Commands`; there is no Close on a \
             menu bar",
        ),
        (
            "MenuDialog",
            "the bar",
            'n',
            "the design names the menu `Net`; there is no Cancel on a menu \
             bar - Esc closes it",
        ),
        (
            "MenuDialog",
            "the bar",
            'h',
            "the design names the menu `Show`, and alt+s is already `search` \
            ",
        ),
        (
            "MenuDialog",
            "the bar",
            'o',
            "the design names the menu `Configuration`; c is Commands, so o \
             is the letter left in the word",
        ),
        (
            "ExecuteDialog",
            "the only one",
            'o',
            "no OK button: the four buttons are Execute, Open \
             with..., View and Cancel",
        ),
        (
            "ConflictDialog",
            "file over file",
            'o',
            "no OK button: the six choices are the buttons",
        ),
        (
            "ConflictDialog",
            "directory over directory",
            'o',
            "no OK button: the six choices are the buttons",
        ),
        (
            "CopyMoveDialog",
            "options open",
            'c',
            "no Close button; `Cancel` is `n`",
        ),
        ("FindDialog", "General", 'c', "no Close button"),
        (
            "FindDialog",
            "General",
            'o',
            "no OK button; `Start search` is `a`",
        ),
        (
            "MultiRenameDialog",
            "the only one",
            'h',
            "forced: M, a, t, c, s and e are all taken by the time `Match case` \
             is assigned, and h is the only letter left in its label",
        ),
        ("PackDialog", "the only one", 'c', "no Close button"),
        (
            "ConnectDialog",
            "the only one",
            'o',
            "no OK button; the affirmative is `Connect`, which is the only \
             word on it with an o in",
        ),
    ];

    /// The letter's meaning everywhere it is not a listed exception.
    fn reserved_meaning(letter: char) -> &'static [&'static str] {
        match letter {
            // `No` is a confirmation's negative button, which is `Cancel`'s
            // role under another name.
            'n' => &["Cancel", "No"],
            'c' => &["Close"],
            'h' => &["Help"],
            'o' => &["OK"],
            _ => &[],
        }
    }

    /// `Alt`+digit, the tab strip's accelerator.
    fn alt_digit(d: char) -> DialogKey {
        DialogKey::raw(KeyPress::new(KeyCode::Char(d), KeyModifiers::ALT))
    }

    /// One instance of every [`crate::dialog::Dialog`] in the tree, driven to
    /// every screen it has, and asked for its letters.
    ///
    /// Built from the real constructors and read through
    /// [`crate::dialog::Dialog::mnemonic_letters`] rather than from a second
    /// copy of the tables, so a table that disagrees with the dialog that owns
    /// it is a failure here rather than a comment.
    fn census() -> Vec<Screen> {
        use crate::config::PanelConfig;
        use crate::dialog::{ConfirmDialog, Dialog, InputDialog, MessageDialog};
        use crate::input::DialogId;
        use crate::ops::{
            ConflictRequest, JobFailure, JobId, JobKind, JobStatus, JobSummary, SelectionStats,
        };
        use crate::panel::Side;
        use crate::panel::mask::MaskDialog;
        use crate::remote::Protocol;
        use crate::remote::auth::SecretKind;
        use crate::remote::connect::{ConnectDialog, HostFormDialog};
        use crate::remote::hosts::SavedHost;
        use crate::remote::prompt::SecretDialog;
        use crate::rename::exec::ResultLine;
        use crate::rename::plan::Settings;
        use crate::ui::dialog::conflict::ConflictDialog;
        use crate::ui::dialog::context::ContextMenuDialog;
        use crate::ui::dialog::copy_move::CopyMoveDialog;
        use crate::ui::dialog::drives::DrivesDialog;
        use crate::ui::dialog::execute::ExecuteDialog;
        use crate::ui::dialog::find::{FindDialog, TabKind};
        use crate::ui::dialog::history::HistoryDialog;
        use crate::ui::dialog::menu::MenuDialog;
        use crate::ui::dialog::multirename::MultiRenameDialog;
        use crate::ui::dialog::openwith::OpenWithDialog;
        use crate::ui::dialog::pack::PackDialog;
        use crate::ui::dialog::progress::ProgressDialog;
        use crate::ui::dialog::queue::QueueDialog;
        use crate::ui::dialog::rename::RenameDialog;
        use crate::ui::dialog::renameresult::RenameResultDialog;
        use crate::ui::dialog::resize::ResizeDialog;
        use crate::ui::dialog::summary::SummaryDialog;
        use crate::vfs::{Entry, VfsPath};
        use std::collections::{HashMap, HashSet};
        use std::time::Duration;

        let mut out: Vec<Screen> = Vec::new();
        let mut add = |dialog: &'static str, screen: &'static str, d: &dyn Dialog| {
            out.push(Screen {
                dialog,
                screen,
                letters: d.mnemonic_letters(),
            });
        };

        // The two primitives whose labels are the caller's and the two that
        // declare no letters at all.
        let confirm = ConfirmDialog::new(DialogId::ConfirmDelete, "Delete", vec!["3 files".into()]);
        add("ConfirmDialog", "Yes / No", &confirm);
        let delete = ConfirmDialog::new(DialogId::ConfirmDelete, "Delete", vec!["3 files".into()])
            .with_buttons("Delete", "Cancel");
        add("ConfirmDialog", "Delete / Cancel", &delete);
        let input = InputDialog::new(DialogId::Mkdir, "Create", "New directory name:", "");
        add("InputDialog", "the only one", &input);
        let message = MessageDialog::new("Note", vec!["nothing to do".into()]);
        add("MessageDialog", "the only one", &message);
        add("QueueDialog", "the only one", &QueueDialog::new(Vec::new()));

        add(
            "MaskDialog",
            "the only one",
            &MaskDialog::new(DialogId::SelectMask, "*.rs"),
        );

        // The Find dialog answers per tab, and the tab is reached the way a
        // user reaches it: `Alt`+digit.
        let mut find = FindDialog::new(VfsPath::local("/home/thorin/dev"), Vec::new(), &state());
        assert_eq!(find.tab(), TabKind::General, "it opens on General");
        add("FindDialog", "General", &find);
        find.handle_key(&alt_digit('2'));
        assert_eq!(find.tab(), TabKind::Advanced, "Alt+2 opens Advanced");
        add("FindDialog", "Advanced", &find);
        find.handle_key(&alt_digit('3'));
        assert_eq!(find.tab(), TabKind::LoadSave, "Alt+3 opens Load/Save");
        add("FindDialog", "Load/Save", &find);

        // The copy dialog's conflict stepper is `Accel::Absent` while the
        // options row is collapsed, so it has two screens.
        let stats = SelectionStats {
            bytes: 19_058_360_320,
            files: 523,
            dirs: 95,
            unsized_dirs: 0,
        };
        let mut copy = CopyMoveDialog::new(
            JobKind::Move,
            3,
            "/srv/media/*.*",
            stats,
            &PanelConfig::default(),
        );
        add("CopyMoveDialog", "options collapsed", &copy);
        copy.handle_key(&alt('s'));
        add("CopyMoveDialog", "options open", &copy);

        // Three conflict shapes, because three sets of choices.
        let request = |both_dirs: bool, dest_is_dir: bool| {
            Box::new(ConflictRequest {
                source: VfsPath::local("/tmp/report.txt"),
                dest: VfsPath::local("/srv/media/report.txt"),
                source_size: 2_345_678,
                dest_size: 1_234_567,
                source_mtime: None,
                dest_mtime: None,
                both_dirs,
                dest_is_dir,
            })
        };
        for (screen, both_dirs, dest_is_dir) in [
            ("file over file", false, false),
            ("directory over directory", true, true),
            ("directory over file", false, true),
        ] {
            let d = ConflictDialog::new(
                JobId(1),
                request(both_dirs, dest_is_dir),
                "report (2).txt",
                &PanelConfig::default(),
            );
            add("ConflictDialog", screen, &d);
        }

        let dir = VfsPath::local("/srv/media");
        let rows: Vec<(Entry, VfsPath)> = ["a.txt", "b.txt"]
            .iter()
            .map(|n| (Entry::file(*n), dir.join(n)))
            .collect();
        let mut siblings: HashMap<VfsPath, HashSet<String>> = HashMap::new();
        siblings.insert(
            dir.clone(),
            ["a.txt".to_string(), "b.txt".to_string()].into(),
        );
        add(
            "MultiRenameDialog",
            "the only one",
            &MultiRenameDialog::new(rows, siblings, Settings::reset(), false),
        );

        add(
            "PackDialog",
            "the only one",
            &PackDialog::new(3, "/srv/media/archive.tar.gz"),
        );
        add("ResizeDialog", "the only one", &ResizeDialog::new(12));
        add(
            "ProgressDialog",
            "the only one",
            &ProgressDialog::new(JobStatus::queued(JobId(3), JobKind::Copy), 1024 * 1024),
        );
        add(
            "RenameDialog",
            "the only one",
            &RenameDialog::new("notes.txt", false, Vec::new()),
        );
        add(
            "RenameResultDialog",
            "the only one",
            &RenameResultDialog::new(vec![ResultLine {
                from: "/srv/media/a.txt".to_string(),
                to: "b.txt".to_string(),
                error: None,
            }]),
        );

        // A clean summary has no `Retry failures` button, so it has a second
        // screen with one fewer letter.
        let summary = |failures: usize| JobSummary {
            kind: JobKind::Copy,
            files_done: 197,
            dirs_done: 3,
            bytes_done: 1_000,
            skipped: 0,
            failures: (0..failures)
                .map(|i| JobFailure {
                    path: VfsPath::local(format!("/srv/media/file-{i}.txt")),
                    error: "Permission denied (os error 13)".to_string(),
                })
                .collect(),
            cancelled: false,
            elapsed: Duration::from_secs(12),
            sized: Vec::new(),
            differing: Vec::new(),
        };
        add(
            "SummaryDialog",
            "clean",
            &SummaryDialog::new(JobId(4), summary(0)),
        );
        add(
            "SummaryDialog",
            "with failures",
            &SummaryDialog::new(JobId(5), summary(2)),
        );

        // the five screens. The host
        // book is not empty, because `Edit` and `Delete` are buttons whatever
        // is in it and a populated list is the screen a user sees.
        let hosts = vec![SavedHost {
            label: "nas".to_string(),
            protocol: Protocol::Sftp,
            host: "nas.local".to_string(),
            port: 2222,
            username: "thorin".to_string(),
            ..SavedHost::default()
        }];
        add(
            "ConnectDialog",
            "the only one",
            &ConnectDialog::new(hosts.clone(), Protocol::Sftp, "thorin".to_string(), true),
        );
        add(
            "HostFormDialog",
            "the only one",
            &HostFormDialog::new(hosts, None),
        );
        // The keyring checkbox is on the screen only where the host opted in
        // and a store exists, which is the screen with every letter on it.
        add(
            "SecretDialog",
            "the only one",
            &SecretDialog::new(
                SecretKind::Password {
                    authority: "sftp://thorin@nas.local:2222".to_string(),
                },
                true,
            ),
        );
        // the disconnect prompt and the unknown-host
        // prompt: both `ConfirmDialog`s, so their letters come from their
        // labels.
        add(
            "ConfirmDialog",
            "Disconnect / Cancel",
            &ConfirmDialog::new(
                DialogId::ConfirmDisconnect,
                "Disconnect",
                vec!["Disconnect from nas.local?".to_string()],
            )
            .with_buttons("Disconnect", "Cancel"),
        );
        add(
            "ConfirmDialog",
            "Accept / Cancel",
            &ConfirmDialog::new(
                DialogId::HostKey,
                "Unknown host key",
                vec!["SHA256:cSZl24ZIbs09gyHUOKCL81rlk8QGx/vH2e/T7WPcEuk".to_string()],
            )
            .with_buttons("Accept", "Cancel"),
        );

        // the popup, in both of its shapes: `Alt+F1`'s devices
        // over the hotlist, and `Ctrl+D`'s hotlist alone.
        let hot = vec![crate::devices::hotlist::HotlistRow {
            entry: crate::devices::hotlist::HotlistEntry {
                label: "dev".to_string(),
                path: "/home/thorin/dev".to_string(),
            },
            resolved: Some(std::path::PathBuf::from("/home/thorin/dev")),
            missing: None,
        }];
        let device = crate::devices::Device {
            mount_point: "/".to_string(),
            label: "root".to_string(),
            fs_type: "ext4".to_string(),
            free: 823_000_000_000,
            total: 929_000_000_000,
            removable: false,
            read_only: false,
        };
        add(
            "DrivesDialog",
            "devices and hotlist",
            &DrivesDialog::devices(Side::Left, vec![device], hot.clone()),
        );
        add("DrivesDialog", "hotlist only", &DrivesDialog::hotlist(hot));

        // the menu bar. Its rows are built from the live keymap, so
        // it needs an `App` - the same headless one every other model-driven
        // test in the tree uses.
        let menu_app = crate::app::App::headless(
            crate::config::Config::default(),
            crate::config::Keymap::builtin(),
            crate::config::Theme::blue(),
        );
        add(
            "MenuDialog",
            "the bar",
            &MenuDialog::new(crate::ui::dialog::menu::model(&menu_app), 0),
        );

        // the context menu, in both of the shapes:
        // the three kinds that need a real local path are **absent** rather
        // than greyed on a remote or in-archive entry.
        let open_cfg = crate::config::OpenConfig::default();
        for (screen, local) in [("local entry", true), ("remote or in-archive", false)] {
            let items = ContextMenuDialog::items_for(
                &open_cfg,
                "notes.txt",
                "text/plain",
                0o100_644,
                local,
            );
            add(
                "ContextMenuDialog",
                screen,
                &ContextMenuDialog::new("notes.txt".to_string(), 0, items),
            );
        }

        // the execute prompt and the chooser.
        add(
            "ExecuteDialog",
            "the only one",
            &ExecuteDialog::new(
                "build.sh".to_string(),
                412,
                "shell script".to_string(),
                &PanelConfig::default(),
            ),
        );
        add(
            "OpenWithDialog",
            "the only one",
            &OpenWithDialog::new(
                "photo.jpg".to_string(),
                vec![crate::ops::open::DesktopApp {
                    name: "Image Viewer".to_string(),
                    id: "org.gnome.eog.desktop".to_string(),
                    exec: vec!["eog".to_string()],
                }],
            ),
        );

        // the `Alt+F8`.
        add(
            "HistoryDialog",
            "the only one",
            &HistoryDialog::new(&["ls -la".to_string()]),
        );

        out
    }

    #[test]
    fn the_census_covers_every_dialog_in_the_tree() {
        // adding a dialog and forgetting to add
        // it to `census()` is the one hole the compiler does not close, so the
        // count is asserted and adding a dialog is a deliberate two-line edit.
        let screens = census();
        assert_eq!(screens.len(), SCREENS, "one row per dialog screen");
        let mut dialogs: Vec<&str> = screens.iter().map(|s| s.dialog).collect();
        dialogs.sort_unstable();
        dialogs.dedup();
        assert_eq!(
            dialogs.len(),
            DIALOGS,
            "every `impl Dialog` in the tree, once: {dialogs:?}"
        );
        for screen in &screens {
            assert!(
                screen.letters.iter().all(char::is_ascii_lowercase),
                "{} / {}: letters are stored folded to lower case",
                screen.dialog,
                screen.screen
            );
        }
    }

    #[test]
    fn a_mnemonic_is_unique_within_every_dialog_screen() {
        // "Mnemonics are unique within a dialog. A duplicate is a
        // bug rather than a first-one-wins rule, because the second control
        // becomes unreachable silently; it is caught by a test over every
        // dialog rather than by inspection." That test is this one
        // (the design I2).
        for screen in census() {
            let table: Vec<((), char)> = screen.letters.iter().map(|l| ((), *l)).collect();
            assert_eq!(
                duplicate_letter(&table),
                None,
                "{} / {}: two controls share a letter, so one is unreachable",
                screen.dialog,
                screen.screen
            );
        }
    }

    #[test]
    fn no_dialog_spends_a_letter_the_tab_strip_owns() {
        // the design I5, the half that the census can see: no table
        // in the tree contains a digit. The other two halves are
        // `a_mnemonic_is_never_a_digit_so_the_tab_strip_keeps_them` above and
        // `TabStrip::handle`, which claims `Alt`+digit and nothing else.
        for screen in census() {
            for letter in screen.letters {
                assert!(
                    letter.is_ascii_alphabetic(),
                    "{} / {}: `Alt+{letter}` is not a letter",
                    screen.dialog,
                    screen.screen
                );
            }
        }
    }

    #[test]
    fn the_four_reserved_letters_mean_one_thing_each() {
        // `Alt+N` is `Cancel`, `Alt+C` is
        // `Close`, `Alt+H` is `Help` and `Alt+O` is `OK`, everywhere, with the
        // exceptions listed by name in `RESERVED_EXCEPTIONS` so that adding one
        // is a deliberate edit rather than a drift.
        for screen in census() {
            for letter in RESERVED_LETTERS {
                let used = screen.letters.contains(&letter);
                let listed = RESERVED_USES.iter().any(|(dialog, name, l, _)| {
                    *dialog == screen.dialog && *name == screen.screen && *l == letter
                });
                assert_eq!(
                    used,
                    listed,
                    "{} / {}: `Alt+{letter}` is {} by the dialog and {} in RESERVED_USES",
                    screen.dialog,
                    screen.screen,
                    if used { "used" } else { "unused" },
                    if listed { "listed" } else { "absent" }
                );
            }
        }
        for (dialog, screen, letter, means) in RESERVED_USES {
            if reserved_meaning(*letter).contains(means) {
                continue;
            }
            let excused = RESERVED_EXCEPTIONS
                .iter()
                .any(|(d, s, l, _)| d == dialog && s == screen && l == letter);
            assert!(
                excused,
                "{dialog} / {screen}: `Alt+{letter}` means {means:?}, which is not \
                 {:?} and is not a listed exception",
                reserved_meaning(*letter)
            );
        }
        for (dialog, screen, letter, _) in RESERVED_EXCEPTIONS {
            assert!(
                RESERVED_USES
                    .iter()
                    .any(|(d, s, l, _)| d == dialog && s == screen && l == letter),
                "{dialog} / {screen}: `Alt+{letter}` is excused but not used"
            );
        }
    }

    /// The Find dialog's search state, which it reads its history from.
    fn state() -> crate::search::Session {
        crate::search::Session::default()
    }
}
