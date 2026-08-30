//! Which control a key moves to, and what it does there.
//!
//! The dialog has three tabs of controls and one focus, so every key is
//! answered in two steps: which control is focused now, and what that control
//! does with this key. Keeping both in one file is what makes the ring
//! checkable, since a control that can be reached and not left, or left and
//! not reached, is a control the user gets stuck in.
//!
//! A drop-down is a control that eats its own keys while it is open. That is
//! not a mode: it is one more thing the focused control does with a key, which
//! is why the open list is checked here and not before the key arrives.

use super::*;

impl FindDialog {
    /// `Space` on a checkbox or a tri-state.
    pub(super) fn toggle(&mut self, control: Control) -> DialogOutcome {
        if let Some(reason) = self.gate(control).reason() {
            // A greyed control refuses rather than silently doing nothing, so
            // the reason is on the screen.
            self.error = Some(reason.to_string());
            return DialogOutcome::Consumed;
        }
        self.error = None;
        match control {
            Control::NameRegex => self.name_regex = !self.name_regex,
            Control::Restrict => self.restrict = !self.restrict,
            Control::Archives => self.archives = !self.archives,
            Control::FindText => self.find_text = !self.find_text,
            Control::WholeWords => self.whole_words = !self.whole_words,
            Control::CaseSensitive => self.case_sensitive = !self.case_sensitive,
            // `Hex` and `RegEx` are mutually exclusive. Ticking one
            // unticks the other rather than refusing, because the user has
            // just said which one they mean.
            Control::TextRegex => {
                self.text_regex = !self.text_regex;
                if self.text_regex {
                    self.hex = false;
                }
            }
            Control::Hex => {
                self.hex = !self.hex;
                if self.hex {
                    self.text_regex = false;
                }
            }
            Control::Inverted => self.inverted = !self.inverted,
            Control::Utf8 => self.charsets.utf8 = !self.charsets.utf8,
            Control::Utf16 => self.charsets.utf16 = !self.charsets.utf16,
            Control::Latin1 => self.charsets.latin1 = !self.charsets.latin1,
            Control::Cp437 => self.charsets.cp437 = !self.charsets.cp437,
            Control::AttrDirectories => self.attrs.directories = self.attrs.directories.next(),
            Control::AttrHidden => self.attrs.hidden = self.attrs.hidden.next(),
            Control::AttrExecutable => self.attrs.executable = self.attrs.executable.next(),
            Control::AttrSymlinks => self.attrs.symlinks = self.attrs.symlinks.next(),
            Control::AttrReadOnly => self.attrs.read_only = self.attrs.read_only.next(),
            Control::Tabs
            | Control::Name
            | Control::Root
            | Control::RootAdd
            | Control::RootList
            | Control::Devices
            | Control::Depth
            | Control::Text
            | Control::SizeMin
            | Control::SizeMax
            | Control::Modified
            | Control::After
            | Control::Before
            | Control::Days
            | Control::SavedList
            | Control::Load
            | Control::SaveAs
            | Control::Delete
            | Control::Start
            | Control::Cancel
            | Control::Help => return DialogOutcome::Ignored,
        }
        DialogOutcome::Consumed
    }

    /// `Enter`, or `Space` on a button: act on the focused control.
    pub(super) fn activate(&mut self) -> DialogOutcome {
        self.activate_control(self.focused())
    }

    /// Act on one named control, whether or not it is the focused one.
    ///
    /// Split out of [`FindDialog::activate`] for the design's
    /// [`Accelerated::press`], which is handed the control the letter named
    /// rather than reading the ring back. The two agree because
    /// [`Accelerated::mnemonic_key`] moves focus first, and saying so in the
    /// signature is what keeps them from drifting.
    pub(super) fn activate_control(&mut self, control: Control) -> DialogOutcome {
        match control {
            Control::Cancel => DialogOutcome::Cancel,
            Control::Help => Self::help(),
            Control::Devices => Self::devices(),
            Control::RootAdd => self.add_root(),
            Control::Load => self.load_selected(),
            Control::SaveAs => self.save_prompt(),
            Control::Delete => self.delete_selected(),
            // A form accepts from its fields and its checkboxes, so the common
            // case is "type the mask, press Enter" - the same rule the copy
            // dialog states.
            Control::Tabs
            | Control::Name
            | Control::NameRegex
            | Control::Root
            | Control::RootList
            | Control::Restrict
            | Control::Archives
            | Control::Depth
            | Control::FindText
            | Control::Text
            | Control::WholeWords
            | Control::CaseSensitive
            | Control::TextRegex
            | Control::Hex
            | Control::Inverted
            | Control::Utf8
            | Control::Utf16
            | Control::Latin1
            | Control::Cp437
            | Control::SizeMin
            | Control::SizeMax
            | Control::Modified
            | Control::After
            | Control::Before
            | Control::Days
            | Control::AttrDirectories
            | Control::AttrHidden
            | Control::AttrExecutable
            | Control::AttrSymlinks
            | Control::AttrReadOnly
            | Control::SavedList
            | Control::Start => self.accept(),
        }
    }

    /// the `>>`: one more search root.
    fn add_root(&mut self) -> DialogOutcome {
        if self.roots.len() >= MAX_ROOTS {
            self.error = Some(format!("a search takes at most {MAX_ROOTS} roots"));
            return DialogOutcome::Consumed;
        }
        let text = self.root.text().trim().to_string();
        if text.is_empty() {
            self.error = Some("type a path to add first".to_string());
            return DialogOutcome::Consumed;
        }
        self.roots.push(text);
        self.root_cursor = self.roots.len().saturating_sub(1);
        self.error = None;
        // The new entry is what "Search in" now edits, so the second path is
        // typed over the first rather than beside it.
        self.focus(Control::Root);
        DialogOutcome::Consumed
    }

    /// `Delete` on the root list.
    pub(super) fn drop_root(&mut self) -> DialogOutcome {
        if self.roots.len() <= 1 {
            self.error = Some("a search needs somewhere to start".to_string());
            return DialogOutcome::Consumed;
        }
        if self.root_cursor < self.roots.len() {
            self.roots.remove(self.root_cursor);
        }
        self.root_cursor = self.root_cursor.min(self.roots.len().saturating_sub(1));
        self.sync_root_field();
        self.error = None;
        DialogOutcome::Consumed
    }

    /// What the arrows drive here, when they drive a control (see
    /// [`Stepper`]). The one exhaustive match over [`Control`] on that
    /// question.
    pub(super) const fn stepper(control: Control) -> Stepper {
        match control {
            Control::Depth => Stepper::Depth,
            Control::Modified => Stepper::Date,
            Control::RootList => Stepper::Roots,
            Control::SavedList => Stepper::Saved,
            Control::Tabs
            | Control::Name
            | Control::NameRegex
            | Control::Root
            | Control::RootAdd
            | Control::Devices
            | Control::Restrict
            | Control::Archives
            | Control::FindText
            | Control::Text
            | Control::WholeWords
            | Control::CaseSensitive
            | Control::TextRegex
            | Control::Hex
            | Control::Inverted
            | Control::Utf8
            | Control::Utf16
            | Control::Latin1
            | Control::Cp437
            | Control::SizeMin
            | Control::SizeMax
            | Control::After
            | Control::Before
            | Control::Days
            | Control::AttrDirectories
            | Control::AttrHidden
            | Control::AttrExecutable
            | Control::AttrSymlinks
            | Control::AttrReadOnly
            | Control::Load
            | Control::SaveAs
            | Control::Delete
            | Control::Start
            | Control::Cancel
            | Control::Help => Stepper::None,
        }
    }

    /// Step whatever the arrows are driving.
    pub(super) fn step(&mut self, stepper: Stepper, forward: bool) {
        match stepper {
            Stepper::None => {}
            Stepper::Depth => self.step_depth(forward),
            Stepper::Date => self.step_date(forward),
            Stepper::Roots => self.step_root(forward),
            Stepper::Saved => self.step_saved(forward),
        }
    }

    /// Move the focus ring.
    pub(super) fn step_focus(&mut self, forward: bool) {
        if forward {
            self.ring_mut().next();
        } else {
            self.ring_mut().prev();
        }
    }

    /// Move the root-list cursor.
    fn step_root(&mut self, forward: bool) {
        let len = self.roots.len();
        if len == 0 {
            return;
        }
        self.root_cursor = if forward {
            self.root_cursor.saturating_add(1).rem_euclid(len)
        } else {
            self.root_cursor
                .saturating_add(len)
                .saturating_sub(1)
                .rem_euclid(len)
        };
        self.sync_root_field();
    }

    /// Step the "Search in subdirectories" dropdown.
    fn step_depth(&mut self, forward: bool) {
        let len = Depth::CHOICES.len();
        if len == 0 {
            return;
        }
        self.depth = if forward {
            self.depth.saturating_add(1).rem_euclid(len)
        } else {
            self.depth
                .saturating_add(len)
                .saturating_sub(1)
                .rem_euclid(len)
        };
    }

    /// Step the `Modified` radio.
    fn step_date(&mut self, forward: bool) {
        let all = DateChoice::ALL;
        let at = all.iter().position(|c| *c == self.date_choice).unwrap_or(0);
        let len = all.len();
        let next = if forward {
            at.saturating_add(1).rem_euclid(len)
        } else {
            at.saturating_add(len).saturating_sub(1).rem_euclid(len)
        };
        self.date_choice = all.get(next).copied().unwrap_or(DateChoice::Any);
    }

    /// Move the saved-search list cursor.
    fn step_saved(&mut self, forward: bool) {
        if self.saved.is_empty() {
            return;
        }
        let len = self.saved.len();
        self.saved_cursor = if forward {
            self.saved_cursor.saturating_add(1).rem_euclid(len)
        } else {
            self.saved_cursor
                .saturating_add(len)
                .saturating_sub(1)
                .rem_euclid(len)
        };
    }

    /// `Load` on the Load/Save tab: the whole saved search, roots included.
    fn load_selected(&mut self) -> DialogOutcome {
        let Some(entry) = self.saved.get(self.saved_cursor).cloned() else {
            self.error = Some("no saved search to load".to_string());
            return DialogOutcome::Consumed;
        };
        let query = entry.query.to_query(&self.start);
        self.apply(&query, true);
        self.set_tab(TabKind::General);
        DialogOutcome::Consumed
    }

    /// `Delete` on the Load/Save tab.
    fn delete_selected(&mut self) -> DialogOutcome {
        if self.saved_cursor >= self.saved.len() {
            self.error = Some("no saved search to delete".to_string());
            return DialogOutcome::Consumed;
        }
        self.saved.remove(self.saved_cursor);
        self.saved_cursor = self.saved_cursor.min(self.saved.len().saturating_sub(1));
        self.saved_dirty = true;
        self.error = None;
        DialogOutcome::Consumed
    }

    /// `Save as…`: the name prompt, pushed on top of this dialog exactly as
    /// `+ F7` is pushed on top of the copy dialog.
    fn save_prompt(&self) -> DialogOutcome {
        DialogOutcome::Push(Box::new(InputDialog::new(
            DialogId::SaveSearch,
            "Save search",
            "Save this search as:",
            self.saved
                .get(self.saved_cursor)
                .map_or("", |s| s.name.as_str()),
        )))
    }

    /// The `Devices` button.
    ///
    /// the design says it "opens the same picker as `Alt+F1`", and that
    /// picker is the design, which the design puts in v0.7. The button says
    /// which milestone brings it rather than doing nothing, the same way the
    /// copy dialog's `Tree` button does.
    fn devices() -> DialogOutcome {
        DialogOutcome::Push(Box::new(MessageDialog::line(
            "Devices",
            format!(
                "the device picker: not implemented until {}",
                Milestone::V07
            ),
        )))
    }

    /// The `Help` button and `F1`.
    ///
    /// the context-sensitive help is a v0.7 milestone and a dialog
    /// cannot open a viewer - [`Dialog::handle_key`] has no `&mut App` by
    /// design - so this is the rules in the box that is available.
    pub(super) fn help() -> DialogOutcome {
        DialogOutcome::Push(Box::new(MessageDialog::new(
            "Find files",
            vec![
                "Search for   a name mask: *.rs, or several: *.rs *.toml".to_string(),
                "             RegEx reads it as an unanchored regex instead.".to_string(),
                "Search in    where to start; >> adds another root.".to_string(),
                "Find text    a checkbox. Content search is explicitly on,".to_string(),
                "             never inferred from the field being non-empty.".to_string(),
                "Subdirs      none, all, or a number of levels.".to_string(),
                "Advanced     size, modification date and attributes.".to_string(),
                "Load/Save    named searches, kept in searches.toml.".to_string(),
                String::new(),
                "Results fill the active panel as they are found; Esc stops".to_string(),
                "the walk and keeps what was found, and Esc again leaves.".to_string(),
            ],
        )))
    }

    /// `Ctrl+Down` on a combo box.
    pub(super) fn open_dropdown(&mut self, control: Control) -> DialogOutcome {
        let items = match Self::field_id(control) {
            Some(FieldId::Name) => self.history.names.clone(),
            Some(FieldId::Text) => self.history.texts.clone(),
            Some(FieldId::Root) => self.history.roots.clone(),
            // The other five fields are not combo boxes: the design gives a
            // history to the three that a search is repeated from.
            Some(
                FieldId::SizeMin
                | FieldId::SizeMax
                | FieldId::After
                | FieldId::Before
                | FieldId::Days,
            )
            | None => Vec::new(),
        };
        if items.is_empty() {
            self.error = Some("no history yet".to_string());
            return DialogOutcome::Consumed;
        }
        self.dropdown = Some(Dropdown {
            control,
            cursor: 0,
            items,
        });
        self.error = None;
        DialogOutcome::Consumed
    }

    /// One key while a dropdown is open. It consumes everything, because a
    /// list that closed on the key you meant for it would be worse than none.
    pub(super) fn dropdown_key(&mut self, key: &DialogKey) -> DialogOutcome {
        let Some(open) = self.dropdown.as_mut() else {
            return DialogOutcome::Ignored;
        };
        let len = open.items.len();
        match key.press.code {
            KeyCode::Up => {
                open.cursor = open
                    .cursor
                    .saturating_add(len)
                    .saturating_sub(1)
                    .rem_euclid(len);
            }
            KeyCode::Down => {
                open.cursor = open.cursor.saturating_add(1).rem_euclid(len);
            }
            KeyCode::Home => open.cursor = 0,
            KeyCode::End => open.cursor = len.saturating_sub(1),
            KeyCode::Enter => {
                let picked = open.items.get(open.cursor).cloned();
                let control = open.control;
                self.dropdown = None;
                if let Some(text) = picked {
                    if let Some(field) = self.field(control) {
                        field.set_text(text);
                    }
                    if Self::field_id(control) == Some(FieldId::Root) {
                        self.sync_root_entry();
                    }
                }
            }
            _ => self.dropdown = None,
        }
        DialogOutcome::Consumed
    }

    /// Which field a control edits, if it edits one.
    ///
    /// The one exhaustive match over [`Control`] on that question; see
    /// [`FieldId`].
    pub(super) const fn field_id(control: Control) -> Option<FieldId> {
        match control {
            Control::Name => Some(FieldId::Name),
            Control::Root => Some(FieldId::Root),
            Control::Text => Some(FieldId::Text),
            Control::SizeMin => Some(FieldId::SizeMin),
            Control::SizeMax => Some(FieldId::SizeMax),
            Control::After => Some(FieldId::After),
            Control::Before => Some(FieldId::Before),
            Control::Days => Some(FieldId::Days),
            Control::Tabs
            | Control::NameRegex
            | Control::RootAdd
            | Control::RootList
            | Control::Devices
            | Control::Restrict
            | Control::Archives
            | Control::Depth
            | Control::FindText
            | Control::WholeWords
            | Control::CaseSensitive
            | Control::TextRegex
            | Control::Hex
            | Control::Inverted
            | Control::Utf8
            | Control::Utf16
            | Control::Latin1
            | Control::Cp437
            | Control::Modified
            | Control::AttrDirectories
            | Control::AttrHidden
            | Control::AttrExecutable
            | Control::AttrSymlinks
            | Control::AttrReadOnly
            | Control::SavedList
            | Control::Load
            | Control::SaveAs
            | Control::Delete
            | Control::Start
            | Control::Cancel
            | Control::Help => None,
        }
    }

    /// Is this control a button - something `Enter` and `Space` press rather
    /// than toggle?
    ///
    /// Exhaustive for the same reason [`FindDialog::field_id`] is: a control
    /// added later has to be classified here rather than defaulting to
    /// "starts the search".
    pub(super) const fn is_button(control: Control) -> bool {
        match control {
            Control::RootAdd
            | Control::Devices
            | Control::Load
            | Control::SaveAs
            | Control::Delete
            | Control::Start
            | Control::Cancel
            | Control::Help => true,
            Control::Tabs
            | Control::Name
            | Control::NameRegex
            | Control::Root
            | Control::RootList
            | Control::Restrict
            | Control::Archives
            | Control::Depth
            | Control::FindText
            | Control::Text
            | Control::WholeWords
            | Control::CaseSensitive
            | Control::TextRegex
            | Control::Hex
            | Control::Inverted
            | Control::Utf8
            | Control::Utf16
            | Control::Latin1
            | Control::Cp437
            | Control::SizeMin
            | Control::SizeMax
            | Control::Modified
            | Control::After
            | Control::Before
            | Control::Days
            | Control::AttrDirectories
            | Control::AttrHidden
            | Control::AttrExecutable
            | Control::AttrSymlinks
            | Control::AttrReadOnly
            | Control::SavedList => false,
        }
    }

    /// The field a control edits, when it edits one.
    pub(super) const fn field_of(&self, control: Control) -> Option<&Field> {
        match Self::field_id(control) {
            Some(FieldId::Name) => Some(&self.name),
            Some(FieldId::Root) => Some(&self.root),
            Some(FieldId::Text) => Some(&self.text),
            Some(FieldId::SizeMin) => Some(&self.size_min),
            Some(FieldId::SizeMax) => Some(&self.size_max),
            Some(FieldId::After) => Some(&self.after),
            Some(FieldId::Before) => Some(&self.before),
            Some(FieldId::Days) => Some(&self.days),
            None => None,
        }
    }

    /// The field a control edits, mutably.
    pub(super) const fn field(&mut self, control: Control) -> Option<&mut Field> {
        match Self::field_id(control) {
            Some(FieldId::Name) => Some(&mut self.name),
            Some(FieldId::Root) => Some(&mut self.root),
            Some(FieldId::Text) => Some(&mut self.text),
            Some(FieldId::SizeMin) => Some(&mut self.size_min),
            Some(FieldId::SizeMax) => Some(&mut self.size_max),
            Some(FieldId::After) => Some(&mut self.after),
            Some(FieldId::Before) => Some(&mut self.before),
            Some(FieldId::Days) => Some(&mut self.days),
            None => None,
        }
    }

    /// Typing into a greyed date field selects the mode that reads it.
    ///
    /// The control the user last touched is the one that means it - the rule
    /// the pack dialog already states about its format selector. It is not
    /// applied to `Find text`, which the design requires to be explicit: a
    /// mode among three date fields is unambiguous, and "search the contents
    /// of every file in this tree" is not.
    pub(super) fn claim_date_mode(&mut self, control: Control) {
        match Self::field_id(control) {
            Some(FieldId::After | FieldId::Before) => self.date_choice = DateChoice::Between,
            Some(FieldId::Days) => self.date_choice = DateChoice::Newer,
            Some(
                FieldId::Name | FieldId::Root | FieldId::Text | FieldId::SizeMin | FieldId::SizeMax,
            )
            | None => {}
        }
    }
}
