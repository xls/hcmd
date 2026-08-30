//! Where each control is drawn, and what it looks like there.
//!
//!
//! One set of rectangles serves both the paint and the caret: `render` draws
//! into them and `cursor` puts the caret in them, so the caret cannot land a
//! row away from the text it belongs to. That is why the geometry is functions
//! of the area rather than numbers written twice.
//!
//! A label's `Alt` mnemonic is underlined from the same table the key handler
//! reads, so what a label promises and what a key does cannot drift apart. A
//! control with no mnemonic draws as plain text.

use super::*;

impl FindDialog {
    /// The rectangle a field is drawn in, so `render` and `cursor` agree.
    ///
    /// The row index is the layout in one place: `render` draws into these
    /// rectangles and `cursor` puts the caret in them, so the caret cannot
    /// land a row away from the text it belongs to.
    pub(super) fn field_rect(&self, area: Rect, control: Control) -> Option<Rect> {
        let id = Self::field_id(control)?;
        let (index, label, trailing) = match id {
            FieldId::Name => (1u16, LABEL_WIDTH, side_button_width("[ ] RegEx")),
            FieldId::Root => (2, LABEL_WIDTH, side_button_width("[>>]")),
            FieldId::Text => (6, find_text_width(), 0),
            FieldId::SizeMin => (1, LABEL_WIDTH, 0),
            FieldId::SizeMax => (2, LABEL_WIDTH, 0),
            FieldId::After | FieldId::Before => (4, LABEL_WIDTH, 0),
            FieldId::Days => (5, LABEL_WIDTH, 0),
        };
        let rect = row(area, index)?;
        // The two date fields share a row, so `after` takes the left half and
        // `before` the right.
        let rect = match id {
            FieldId::After => half(rect, true),
            FieldId::Before => half(rect, false),
            FieldId::Name
            | FieldId::Root
            | FieldId::Text
            | FieldId::SizeMin
            | FieldId::SizeMax
            | FieldId::Days => rect,
        };
        let (_, field, _) = columns(rect, label, trailing);
        (field.width > 0).then_some(field)
    }
}

impl FindDialog {
    /// A control's label, with its `Alt` mnemonic underlined.
    ///
    /// One helper rather than the letter looked up at fifteen call sites: the
    /// table is the key handler's, so a label and the key that reaches it
    /// cannot drift apart. A control with no mnemonic - only the design's
    /// `>>` glyph - draws as plain text.
    fn draw_label(
        &self,
        f: &mut Frame,
        rect: Rect,
        label: &str,
        control: Control,
        text_style: Style,
        style: &DialogStyle,
    ) {
        match self.mnemonic_of(control) {
            Some(letter) => draw_mnemonic(f, rect, label, letter, text_style, style.ascii),
            None => draw_text(f, rect, label, text_style, style.ascii),
        }
    }

    /// A field, drawn greyed when its control is disabled.
    ///
    /// A greyed field is drawn with the dialog's body colours rather than the
    /// input slot's, which is what "greyed out" means on a terminal with no
    /// grey: it stops looking like somewhere to type.
    fn draw_field(&self, f: &mut Frame, rect: Rect, control: Control, style: &DialogStyle) {
        let Some(field) = self.field_of(control) else {
            return;
        };
        if self.enabled(control) {
            field.render(f, rect, style);
        } else {
            draw_text(f, rect, field.text(), style.body(), style.ascii);
        }
    }

    /// The General tab.
    pub(super) fn render_general(
        &self,
        f: &mut Frame,
        area: Rect,
        style: &DialogStyle,
        focused: Control,
    ) {
        let body = style.body();
        let pick = |control: Control| style.button(focused == control);

        // every label below shows its `Alt` mnemonic underlined,
        // and the letter comes from the one table the key handler reads.
        let letter = |control: Control| self.mnemonic_of(control);
        if let Some(rect) = row(area, 1) {
            let (label, field, trailing) =
                columns(rect, LABEL_WIDTH, side_button_width("[ ] RegEx"));
            self.draw_label(f, label, "Search for", Control::Name, body, style);
            self.draw_field(f, field, Control::Name, style);
            draw_mnemonic_pieces(
                f,
                trailing,
                &[Piece::new(
                    checkbox("RegEx", self.name_regex, style.ascii),
                    letter(Control::NameRegex),
                    pick(Control::NameRegex),
                    focused == Control::NameRegex,
                )],
                body,
            );
        }
        if let Some(rect) = row(area, 2) {
            let (label, field, trailing) = columns(rect, LABEL_WIDTH, side_button_width("[>>]"));
            self.draw_label(f, label, "Search in", Control::Root, body, style);
            self.draw_field(f, field, Control::Root, style);
            // The one control with no mnemonic: its label is a glyph, and
            // the underline needs a letter (see `fields`).
            draw_text(f, trailing, " [>>]", pick(Control::RootAdd), style.ascii);
        }
        if let Some(rect) = row(area, 3) {
            let (label, list, trailing) =
                columns(rect, LABEL_WIDTH, side_button_width("[ Devices ]"));
            self.draw_label(f, label, "Roots", Control::RootList, body, style);
            draw_text(
                f,
                list,
                &self.roots_line(),
                pick(Control::RootList),
                style.ascii,
            );
            draw_mnemonic_pieces(
                f,
                trailing,
                &[Piece::new(
                    " [ Devices ]",
                    letter(Control::Devices),
                    pick(Control::Devices),
                    focused == Control::Devices,
                )],
                body,
            );
        }
        if let Some(rect) = row(area, 4) {
            let restrict = checkbox(
                "Only search in selected directories/files",
                self.restrict && self.restrict_available(),
                style.ascii,
            );
            let archives = checkbox("Search archives", self.archives, style.ascii);
            draw_mnemonic_pieces(
                f,
                rect,
                &[
                    Piece::new(
                        restrict,
                        letter(Control::Restrict),
                        self.grey(pick(Control::Restrict), Control::Restrict, style),
                        focused == Control::Restrict,
                    ),
                    Piece::new(
                        archives,
                        letter(Control::Archives),
                        pick(Control::Archives),
                        focused == Control::Archives,
                    ),
                ],
                body,
            );
        }
        if let Some(rect) = row(area, 5) {
            let (label, value, _) = columns(rect, LABEL_WIDTH, 0);
            self.draw_label(f, label, "Subdirs", Control::Depth, body, style);
            let text = format!(
                "{} {} {}",
                if style.ascii { "<" } else { "\u{2039}" },
                self.depth().label(),
                if style.ascii { ">" } else { "\u{203a}" },
            );
            draw_text(f, value, &text, pick(Control::Depth), style.ascii);
        }
        if let Some(rect) = row(area, 6) {
            let (label, field, _) = columns(rect, find_text_width(), 0);
            draw_mnemonic_pieces(
                f,
                label,
                &[Piece::new(
                    checkbox("Find text", self.find_text, style.ascii),
                    letter(Control::FindText),
                    pick(Control::FindText),
                    focused == Control::FindText,
                )],
                body,
            );
            self.draw_field(f, field, Control::Text, style);
        }
        if let Some(rect) = row(area, 7) {
            let pieces = [
                (
                    checkbox("Whole words only", self.whole_words, style.ascii),
                    Control::WholeWords,
                ),
                (
                    checkbox("Case sensitive", self.case_sensitive, style.ascii),
                    Control::CaseSensitive,
                ),
                (
                    checkbox("RegEx", self.text_regex, style.ascii),
                    Control::TextRegex,
                ),
                (checkbox("Hex", self.hex, style.ascii), Control::Hex),
            ];
            self.draw_gated(f, rect, &pieces, style, focused);
        }
        if let Some(rect) = row(area, 8) {
            let pieces = [(
                checkbox(
                    "Find files NOT containing the text",
                    self.inverted,
                    style.ascii,
                ),
                Control::Inverted,
            )];
            self.draw_gated(f, rect, &pieces, style, focused);
        }
        if let Some(rect) = row(area, 9) {
            let pieces = [
                (
                    checkbox("UTF-8", self.charsets.utf8, style.ascii),
                    Control::Utf8,
                ),
                (
                    checkbox("UTF-16", self.charsets.utf16, style.ascii),
                    Control::Utf16,
                ),
                (
                    checkbox("Latin-1 / windows-1252", self.charsets.latin1, style.ascii),
                    Control::Latin1,
                ),
                (
                    checkbox("CP437 (DOS)", self.charsets.cp437, style.ascii),
                    Control::Cp437,
                ),
            ];
            self.draw_gated(f, rect, &pieces, style, focused);
        }
    }

    /// One row of controls that "Find text" gates.
    fn draw_gated(
        &self,
        f: &mut Frame,
        rect: Rect,
        pieces: &[(String, Control)],
        style: &DialogStyle,
        focused: Control,
    ) {
        let drawn: Vec<Piece> = pieces
            .iter()
            .map(|(label, control)| {
                Piece::new(
                    label.clone(),
                    self.mnemonic_of(*control),
                    self.grey(style.button(focused == *control), *control, style),
                    focused == *control,
                )
            })
            .collect();
        draw_mnemonic_pieces(f, rect, &drawn, style.body());
    }

    /// `chosen` when the control is live, the dialog's dimmest body colour
    /// when it is greyed.
    fn grey(&self, chosen: Style, control: Control, style: &DialogStyle) -> Style {
        if self.enabled(control) {
            chosen
        } else {
            Style::new().fg(style.border).bg(style.bg)
        }
    }

    /// The root list line: the focused root, and how many others there are.
    pub(super) fn roots_line(&self) -> String {
        let focused = self
            .roots
            .get(self.root_cursor)
            .map_or_else(String::new, |r| fold_home(r.trim()));
        let others = self.roots.len().saturating_sub(1);
        if others == 0 {
            focused
        } else {
            format!("{focused}  +{others}")
        }
    }

    /// The Advanced tab.
    pub(super) fn render_advanced(
        &self,
        f: &mut Frame,
        area: Rect,
        style: &DialogStyle,
        focused: Control,
    ) {
        let body = style.body();
        let pick = |control: Control| style.button(focused == control);

        let letter = |control: Control| self.mnemonic_of(control);
        for (index, control, label) in [
            (1u16, Control::SizeMin, "Size >="),
            (2, Control::SizeMax, "Size <="),
        ] {
            if let Some(rect) = row(area, index) {
                let (label_rect, field, _) = columns(rect, LABEL_WIDTH, 0);
                self.draw_label(f, label_rect, label, control, body, style);
                self.draw_field(f, field, control, style);
            }
        }
        if let Some(rect) = row(area, 3) {
            let (label, value, _) = columns(rect, LABEL_WIDTH, 0);
            self.draw_label(f, label, "Modified", Control::Modified, body, style);
            let choices: Vec<Piece> = DateChoice::ALL
                .iter()
                .map(|choice| {
                    Piece::new(
                        radio(choice.label(), *choice == self.date_choice),
                        None,
                        pick(Control::Modified),
                        // One control drawn as three values: the radio as a
                        // whole is what the focus is on, so no single value
                        // may claim the row's one focused piece.
                        false,
                    )
                })
                .collect();
            draw_mnemonic_pieces(f, value, &choices, body);
        }
        if let Some(rect) = row(area, 4) {
            let left = half(rect, true);
            let right = half(rect, false);
            let (label, field, _) = columns(left, LABEL_WIDTH, 0);
            self.draw_label(
                f,
                label,
                "after",
                Control::After,
                self.grey(body, Control::After, style),
                style,
            );
            self.draw_field(f, field, Control::After, style);
            let (label, field, _) = columns(right, LABEL_WIDTH, 0);
            self.draw_label(
                f,
                label,
                "before",
                Control::Before,
                self.grey(body, Control::Before, style),
                style,
            );
            self.draw_field(f, field, Control::Before, style);
        }
        if let Some(rect) = row(area, 5) {
            let (label, field, _) = columns(rect, LABEL_WIDTH, 0);
            self.draw_label(
                f,
                label,
                "days",
                Control::Days,
                self.grey(body, Control::Days, style),
                style,
            );
            self.draw_field(f, field, Control::Days, style);
        }
        if let Some(rect) = row(area, 6) {
            draw_mnemonic_pieces(
                f,
                rect,
                &[
                    Piece::new(
                        tristate("Directories", self.attrs.directories),
                        letter(Control::AttrDirectories),
                        pick(Control::AttrDirectories),
                        focused == Control::AttrDirectories,
                    ),
                    Piece::new(
                        tristate("Hidden", self.attrs.hidden),
                        letter(Control::AttrHidden),
                        pick(Control::AttrHidden),
                        focused == Control::AttrHidden,
                    ),
                    Piece::new(
                        tristate("Executable", self.attrs.executable),
                        letter(Control::AttrExecutable),
                        pick(Control::AttrExecutable),
                        focused == Control::AttrExecutable,
                    ),
                ],
                body,
            );
        }
        if let Some(rect) = row(area, 7) {
            draw_mnemonic_pieces(
                f,
                rect,
                &[
                    Piece::new(
                        tristate("Symlinks", self.attrs.symlinks),
                        letter(Control::AttrSymlinks),
                        pick(Control::AttrSymlinks),
                        focused == Control::AttrSymlinks,
                    ),
                    Piece::new(
                        tristate("Read-only", self.attrs.read_only),
                        letter(Control::AttrReadOnly),
                        pick(Control::AttrReadOnly),
                        focused == Control::AttrReadOnly,
                    ),
                ],
                body,
            );
        }
        if let Some(rect) = row(area, 8) {
            draw_text(
                f,
                rect,
                "Space cycles: ignore, require, forbid",
                body,
                style.ascii,
            );
        }
    }

    /// The one line the design does not ask for and this dialog needs: a tabbed
    /// dialog hides state, and this is the cheapest way to stop a user
    /// concluding the search is broken when they left a filter on last week.
    pub fn advanced_summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Ok(Some(min)) = parse_size(self.size_min.text()) {
            parts.push(format!("size >= {}", ByteSize(min)));
        }
        if let Ok(Some(max)) = parse_size(self.size_max.text()) {
            parts.push(format!("size <= {}", ByteSize(max)));
        }
        match self.date_range() {
            DateRange::Any => {}
            DateRange::Between { after, before } => {
                if let Some(after) = after {
                    parts.push(format!("modified >= {}", format_date(after)));
                }
                if let Some(before) = before {
                    parts.push(format!("modified <= {}", format_date(before)));
                }
            }
            DateRange::NewerThanDays(days) => parts.push(format!("modified < {days} days")),
        }
        for (label, tri) in [
            ("directories", self.attrs.directories),
            ("hidden", self.attrs.hidden),
            ("executable", self.attrs.executable),
            ("symlinks", self.attrs.symlinks),
            ("read-only", self.attrs.read_only),
        ] {
            match tri {
                Tri::Ignore => {}
                Tri::Yes => parts.push(label.to_string()),
                Tri::No => parts.push(format!("not {label}")),
            }
        }
        if parts.is_empty() {
            "no advanced filters".to_string()
        } else {
            parts.join(", ")
        }
    }

    /// The Load/Save tab.
    pub(super) fn render_load_save(
        &self,
        f: &mut Frame,
        area: Rect,
        style: &DialogStyle,
        focused: Control,
    ) {
        let body = style.body();
        if let Some(rect) = row(area, 1) {
            self.draw_label(f, rect, "Saved searches", Control::SavedList, body, style);
        }
        let buttons_row = area.height.saturating_sub(3);
        let first = 2u16;
        let rows = buttons_row.saturating_sub(first);
        if self.saved.is_empty() {
            if let Some(rect) = row(area, first) {
                draw_text(f, rect, "  (none saved yet)", body, style.ascii);
            }
        } else {
            // Scroll so the cursor is always on screen; the list is short and
            // a saved-search file with a thousand entries is not a case worth
            // paging for.
            let rows_usize = usize::from(rows).max(1);
            let top = self
                .saved_cursor
                .saturating_add(1)
                .saturating_sub(rows_usize);
            for (offset, entry) in self.saved.iter().skip(top).take(rows_usize).enumerate() {
                let Some(index) = u16::try_from(offset).ok().map(|o| first.saturating_add(o))
                else {
                    break;
                };
                let Some(rect) = row(area, index) else { break };
                let selected = top.saturating_add(offset) == self.saved_cursor;
                let line = format!("  {}", entry.name);
                let style_for = if selected && focused == Control::SavedList {
                    style.button(true)
                } else if selected {
                    style.button(false)
                } else {
                    body
                };
                draw_text(f, rect, &line, style_for, style.ascii);
            }
        }
        if let Some(rect) = row(area, buttons_row) {
            let index = button_index(focused, &[Control::Load, Control::SaveAs, Control::Delete]);
            draw_mnemonic_buttons(
                f,
                rect,
                &[
                    ("Load", self.mnemonic_of(Control::Load)),
                    ("Save as...", self.mnemonic_of(Control::SaveAs)),
                    ("Delete", self.mnemonic_of(Control::Delete)),
                ],
                index,
                style,
            );
        }
    }

    /// The history dropdown, over whatever is underneath it.
    pub(super) fn render_dropdown(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        let Some(open) = &self.dropdown else {
            return;
        };
        let Some(field) = self.field_rect(area, open.control) else {
            return;
        };
        let below = area
            .height
            .saturating_sub(field.y.saturating_sub(area.y).saturating_add(1));
        let rows = u16::try_from(open.items.len().min(DROPDOWN_ROWS))
            .unwrap_or(1)
            .min(below);
        if rows == 0 || field.width == 0 {
            return;
        }
        let top = open
            .cursor
            .saturating_add(1)
            .saturating_sub(usize::from(rows));
        let rect = Rect::new(field.x, field.y.saturating_add(1), field.width, rows);
        f.render_widget(Clear, rect);
        for offset in 0..rows {
            let Some(line) = row(rect, offset) else { break };
            let index = top.saturating_add(usize::from(offset));
            let Some(item) = open.items.get(index) else {
                break;
            };
            let chosen = index == open.cursor;
            let style_for = if chosen {
                style.button(true)
            } else {
                style.input()
            };
            draw_text(f, line, item, style_for, style.ascii);
        }
    }
}
