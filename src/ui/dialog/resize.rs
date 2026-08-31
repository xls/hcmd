//! `Shift+R`: the image resize and convert dialog.
//!
//! ```text
//!   ╭ Resize images ──────────────────────────────────────────╮
//!   │holiday.png   4000 x 3000   PNG   11.4 MB                │
//!   │[x] Keep aspect ratio                                    │
//!   │Width:  [50        ]  < percent >                        │
//!   │Height: [50        ]  < percent >                        │
//!   │Fit: < best fit >   Format: < JPEG >   Quality: < 85 >    │
//!   │[x] prefix filename    [thumb_    ]                      │
//!   │[ ] append to filename [          ]                      │
//!   │Saved as: thumb_holiday.jpg   in /photos/small           │
//!   │              [ OK ]   [ Cancel ]                        │
//!   ╰─────────────────────────────────────────────────────────╯
//! ```
//!
//! # The two lines that are not controls
//!
//! **The top line says what is being worked on**, because "50%" is meaningless
//! without "of what": the name, the pixel size, the format and the size on
//! disk, for one image, and a count for several - "12 images" - since a line
//! describing one of twelve would be describing the wrong one eleven times.
//! Where the header could not be read it says so and the dialog still opens: a
//! file this program cannot measure is still one the user may be converting.
//! The facts arrive with the dialog, from [`crate::app::resize`], because
//! reading them is I/O and the keystroke may not do any.
//!
//! **The bottom line is the name the first output will have, and where it
//! goes**, rewritten as the prefix and postfix are typed and as the format is
//! stepped. It is the only place the extension change is visible as a fact
//! rather than as an inference, and with several images it says that the rest
//! follow the same rule.
//!
//! The source name is not repeated on it: the header line directly above
//! already says it, and the width bought by dropping it pays for the
//! directory - which is the other panel's, a convention from `F5` and `F6`
//! that is not visible from inside this dialog at all.
//!
//! **Both name fields are drawn as fields even while their box is unticked**,
//! greyed rather than absent. An empty field drawn as bare text is nothing on
//! screen, and a row reading `[ ] prefix filename` with blank space after it
//! does not show that there is a name waiting to be typed.
//!
//! # One thing this dialog cannot promise
//!
//! A PNG of flat colour or text may come out **larger** than it went in.
//! Resampling invents intermediate colours where there were hard edges, and
//! PNG's filters and deflate live on runs of identical pixels: the smoothing
//! that makes the smaller picture look right is the same thing that makes it
//! compress worse. Every image tool does this, and chasing it with a quantiser
//! would be a different feature with losses of its own.
//!
//! # What is greyed and why
//!
//! **Fit mode is greyed while `Keep aspect ratio` is on**, because it has
//! nothing to decide there: with the ratio kept there is one dimension, and
//! "fit inside the box" and "stretch to the box" are the same instruction.
//! Greyed and still in the `Tab` order, and still refusing in place when it is
//! stepped, which is what every other greyed control in this program does.
//!
//! **The height and its unit are greyed for the same reason**: with the ratio
//! kept there is one dimension, and it is the width. They too refuse in place
//! rather than taking an edit that would never be read.
//!
//! **The quality stepper is greyed for a format that has no option to set.**
//! JPEG has a quality and PNG has a compression effort - `image` calls both of
//! them quality, and so does this label - and a GIF or a BMP has neither.
//!
//! **Each name field is greyed until its own checkbox is ticked**, which is
//! what makes both of them visibly optional and off by default. `Alt+X` and
//! `Alt+A` tick the box and put the caret in the field it gates, so the letter
//! does the whole gesture.
//!
//! WebP is not on the format list at all. It decodes here and does not encode,
//! so offering it could only ever produce a refusal at `OK`; a source in that
//! format with `Same as source` chosen is refused by the job, naming the
//! format, which is the one route to it that cannot be closed up front.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;

use super::checkbox;
use super::field::Field;
use super::row;
use crate::app::resize::ResizeSubject;
use crate::dialog::{
    Accel, Accelerated, Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle, FocusRing,
    Piece, draw_mnemonic, draw_mnemonic_buttons, draw_mnemonic_pieces, draw_text,
};
use crate::input::{DialogId, KeyCode};
use crate::ops::resize::{Amount, FitMode, PngLevel, ResizeSettings, preview_name};
use crate::panel::format::human_size;

/// The formats offered as a target, in the order they are stepped through.
///
/// `None` is `Same as source`. WebP is absent: see the module documentation.
pub const FORMATS: &[Option<image::ImageFormat>] = &[
    None,
    Some(image::ImageFormat::Png),
    Some(image::ImageFormat::Jpeg),
    Some(image::ImageFormat::Gif),
    Some(image::ImageFormat::Bmp),
    Some(image::ImageFormat::Tiff),
    Some(image::ImageFormat::Ico),
    Some(image::ImageFormat::Tga),
];

/// One focusable control, in `Tab` order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// The keep-ratio checkbox.
    KeepRatio,
    /// The width number.
    Width,
    /// Percent or pixels, for the width.
    WidthUnit,
    /// The height number.
    Height,
    /// Percent or pixels, for the height.
    HeightUnit,
    /// Best fit or exact.
    Fit,
    /// The output format.
    Format,
    /// JPEG quality or PNG compression, whichever the format has.
    Quality,
    /// Whether the output name takes a prefix at all.
    PrefixOn,
    /// The name prefix.
    Prefix,
    /// Whether the output name takes a postfix at all.
    PostfixOn,
    /// The name postfix.
    Postfix,
    /// Do it.
    Ok,
    /// Give up.
    Cancel,
}

/// Every control, in `Tab` order. The ring's length is this.
const CONTROLS: &[Control] = &[
    Control::KeepRatio,
    Control::Width,
    Control::WidthUnit,
    Control::Height,
    Control::HeightUnit,
    Control::Fit,
    Control::Format,
    Control::Quality,
    Control::PrefixOn,
    Control::Prefix,
    Control::PostfixOn,
    Control::Postfix,
    Control::Ok,
    Control::Cancel,
];

/// The `Alt` mnemonics for this dialog.
///
/// `h` is reserved program-wide for `Help`, so the height field takes the `g`
/// of its own label. The two unit steppers take `e` and `p`, which are the
/// only two letters `percent` and `pixels` share: any other choice would move
/// the underline every time the stepper is stepped, which is a label that
/// cannot be learned.
///
/// The two name fields have no letter of their own, deliberately: their
/// checkboxes are [`Accel::Gate`]s, so `Alt+X` and `Alt+A` tick the box *and*
/// put the caret in the field. A second letter for the field would be a second
/// way to reach the same caret, one of them leaving the box unticked and the
/// typing therefore ignored.
pub const MNEMONICS: &[(Control, char)] = &[
    (Control::KeepRatio, 'k'),
    (Control::Width, 'w'),
    (Control::WidthUnit, 'e'),
    (Control::Height, 'g'),
    (Control::HeightUnit, 'p'),
    (Control::Fit, 'i'),
    (Control::Format, 'f'),
    (Control::Quality, 'q'),
    (Control::PrefixOn, 'x'),
    (Control::PostfixOn, 'a'),
    (Control::Ok, 'o'),
    (Control::Cancel, 'n'),
];

/// The `Shift+R` dialog.
#[derive(Debug)]
pub struct ResizeDialog {
    subject: ResizeSubject,
    keep_ratio: bool,
    width: Field,
    width_pixels: bool,
    height: Field,
    height_pixels: bool,
    fit: FitMode,
    format: usize,
    jpeg_quality: u8,
    png_compression: PngLevel,
    prefix_on: bool,
    prefix: Field,
    postfix_on: bool,
    postfix: Field,
    error: Option<String>,
    ring: FocusRing,
}

impl ResizeDialog {
    /// A dialog over one described selection, opening on the defaults.
    ///
    /// The subject is read before the dialog is built rather than by it: see
    /// [`crate::app::resize`] for why the keystroke cannot do it.
    pub fn new(subject: ResizeSubject) -> Self {
        let settings = ResizeSettings::default();
        Self {
            subject,
            keep_ratio: settings.keep_ratio,
            width: Field::with_text(amount_text(settings.width)),
            width_pixels: matches!(settings.width, Amount::Pixels(_)),
            height: Field::with_text(amount_text(settings.height)),
            height_pixels: matches!(settings.height, Amount::Pixels(_)),
            fit: settings.fit,
            format: 0,
            jpeg_quality: settings.jpeg_quality,
            png_compression: settings.png_compression,
            prefix_on: false,
            prefix: Field::new(),
            postfix_on: false,
            postfix: Field::new(),
            error: None,
            ring: FocusRing::new(CONTROLS.len()),
        }
    }

    /// What this dialog is about, as it was described to it.
    pub const fn subject(&self) -> &ResizeSubject {
        &self.subject
    }

    /// The top line: what is being worked on.
    ///
    /// One image is described; several are counted, because a line describing
    /// one of twelve would be describing the wrong one eleven times.
    pub fn header(&self) -> String {
        if self.subject.count != 1 {
            return format!("{} images", self.subject.count);
        }
        let mut parts = vec![self.subject.name.clone()];
        match self.subject.dimensions {
            Some((w, h)) => parts.push(format!("{w} x {h} px")),
            // Said, not hidden and not refused: the dialog still opens.
            None => parts.push("dimensions unavailable".to_string()),
        }
        if let Some(format) = self.subject.format {
            parts.push(format_name(Some(format)).to_string());
        }
        parts.push(format!("{}B", human_size(self.subject.size)));
        parts.join("   ")
    }

    /// The bottom line: the name the first output will have.
    ///
    /// Built from the same [`preview_name`] the job names its outputs with, so
    /// the line cannot drift from what lands on the disk.
    pub fn preview(&self) -> String {
        let settings = self.settings();
        let out = preview_name(
            &self.subject.name,
            settings.format.or(self.subject.format),
            &settings,
        );
        // The source name is on the header line directly above, so repeating
        // it here spent the width twice and left no room for the one fact the
        // dialog was still not stating: which directory the output lands in.
        // That is the other panel's, which is a convention rather than
        // something visible from inside this dialog.
        let mut line = format!("Saved as: {out}");
        if !self.subject.destination.is_empty() {
            line.push_str(&format!("   in {}", self.subject.destination));
        }
        match self.subject.count {
            0 | 1 => line,
            n => format!("{line}   (and {} more, the same way)", n.saturating_sub(1)),
        }
    }

    /// Which control has focus.
    pub fn focused(&self) -> Control {
        CONTROLS
            .get(self.ring.index())
            .copied()
            .unwrap_or(Control::KeepRatio)
    }

    /// Whether the output name takes a prefix.
    pub const fn prefix_is_on(&self) -> bool {
        self.prefix_on
    }

    /// Whether the output name takes a postfix.
    pub const fn postfix_is_on(&self) -> bool {
        self.postfix_on
    }

    /// The refusal currently shown, if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The format currently chosen, `None` being `Same as source`.
    pub fn format(&self) -> Option<image::ImageFormat> {
        FORMATS.get(self.format).copied().flatten()
    }

    /// True when the chosen format has an option the quality stepper sets.
    pub fn has_quality(&self) -> bool {
        matches!(
            self.format(),
            Some(image::ImageFormat::Jpeg | image::ImageFormat::Png)
        ) || self.format().is_none()
    }

    /// Everything the dialog holds, as the job wants it.
    pub fn settings(&self) -> ResizeSettings {
        ResizeSettings {
            keep_ratio: self.keep_ratio,
            width: amount_of(self.width.text(), self.width_pixels),
            height: amount_of(self.height.text(), self.height_pixels),
            fit: self.fit,
            format: self.format(),
            jpeg_quality: self.jpeg_quality,
            png_compression: self.png_compression,
            // An unticked box is an empty affix whatever is in the field
            // beside it: the box is the answer, and the field is only what the
            // answer would be.
            prefix: if self.prefix_on {
                self.prefix.text().to_string()
            } else {
                String::new()
            },
            postfix: if self.postfix_on {
                self.postfix.text().to_string()
            } else {
                String::new()
            },
        }
    }

    /// The label of the stepper that says what the numbers mean.
    fn unit_label(pixels: bool) -> &'static str {
        if pixels { "< pixels >" } else { "< percent >" }
    }

    /// The fit stepper's label.
    const fn fit_label(&self) -> &'static str {
        match self.fit {
            FitMode::Best => "Fit: < best fit >",
            FitMode::Exact => "Fit: < exact >",
        }
    }

    /// The format stepper's label.
    fn format_label(&self) -> String {
        format!("Format: < {} >", format_name(self.format()))
    }

    /// The quality stepper's label, which says nothing when there is nothing
    /// to say.
    fn quality_label(&self) -> String {
        match self.format() {
            Some(image::ImageFormat::Png) => format!(
                "Quality: < {} >",
                match self.png_compression {
                    PngLevel::Fast => "fast",
                    PngLevel::Default => "default",
                    PngLevel::Best => "best",
                }
            ),
            // `Same as source` can still land on a JPEG, so the quality is
            // live rather than greyed: it is the setting that will be used if
            // any of the sources turns out to be one.
            None | Some(image::ImageFormat::Jpeg) => format!("Quality: < {} >", self.jpeg_quality),
            Some(_) => "Quality: < n/a >".to_string(),
        }
    }

    /// Step whichever stepper has focus, or move between controls.
    fn step(&mut self, forward: bool) -> DialogOutcome {
        match self.focused() {
            Control::WidthUnit => self.width_pixels = !self.width_pixels,
            Control::HeightUnit => self.height_pixels = !self.height_pixels,
            // Greyed while the ratio is kept, and a greyed control refuses in
            // place rather than moving the focus on: the user pressed a key at
            // this control and is owed an answer about this control.
            Control::Fit if self.keep_ratio => {
                self.error = Some("fit mode has no meaning while the ratio is kept".to_string());
            }
            Control::Fit => {
                self.fit = match self.fit {
                    FitMode::Best => FitMode::Exact,
                    FitMode::Exact => FitMode::Best,
                };
            }
            Control::Format => {
                let n = FORMATS.len();
                self.format = if forward {
                    self.format.saturating_add(1) % n
                } else {
                    self.format.saturating_add(n).saturating_sub(1) % n
                };
                self.error = None;
            }
            Control::Quality => self.step_quality(forward),
            _ => return DialogOutcome::Ignored,
        }
        DialogOutcome::Consumed
    }

    /// Step the per-format option, clamping rather than wrapping: both scales
    /// have ends, and wrapping from best to worst on one keypress is a
    /// surprise.
    fn step_quality(&mut self, forward: bool) {
        match self.format() {
            Some(image::ImageFormat::Png) => {
                self.png_compression = match (self.png_compression, forward) {
                    (PngLevel::Fast, true) | (PngLevel::Best, false) => PngLevel::Default,
                    (PngLevel::Default, true) | (PngLevel::Best, true) => PngLevel::Best,
                    (PngLevel::Default | PngLevel::Fast, false) => PngLevel::Fast,
                };
            }
            None | Some(image::ImageFormat::Jpeg) => {
                self.jpeg_quality = if forward {
                    self.jpeg_quality.saturating_add(5).min(100)
                } else {
                    self.jpeg_quality.saturating_sub(5).max(1)
                };
            }
            Some(_) => {
                self.error = Some("this format has no quality setting".to_string());
            }
        }
    }

    /// The field `control` names, when it is one.
    fn field_mut(&mut self, control: Control) -> Option<&mut Field> {
        match control {
            Control::Width => Some(&mut self.width),
            Control::Height => Some(&mut self.height),
            Control::Prefix => Some(&mut self.prefix),
            Control::Postfix => Some(&mut self.postfix),
            _ => None,
        }
    }

    /// The arrow keys: step whatever has focus, or walk the ring.
    fn arrows(&mut self, key: &DialogKey) -> DialogOutcome {
        match key.press.code {
            KeyCode::Left => match self.step(false) {
                DialogOutcome::Ignored => {
                    self.ring.prev();
                    DialogOutcome::Consumed
                }
                outcome => outcome,
            },
            KeyCode::Right => match self.step(true) {
                DialogOutcome::Ignored => {
                    self.ring.next();
                    DialogOutcome::Consumed
                }
                outcome => outcome,
            },
            KeyCode::Up => {
                self.ring.prev();
                DialogOutcome::Consumed
            }
            KeyCode::Down => {
                self.ring.next();
                DialogOutcome::Consumed
            }
            _ => DialogOutcome::Ignored,
        }
    }

    fn accept(&mut self) -> DialogOutcome {
        if number(self.width.text()) == 0 {
            self.error = Some("a width of nothing is not a size".to_string());
            return DialogOutcome::Consumed;
        }
        if !self.keep_ratio && number(self.height.text()) == 0 {
            self.error = Some("a height of nothing is not a size".to_string());
            return DialogOutcome::Consumed;
        }
        DialogOutcome::Accept(DialogResult::Resize(Box::new(self.settings())))
    }
}

/// What a format is called on screen. `None` is `same as source`.
///
/// One table rather than one per line, so the stepper and the header cannot
/// spell the same format two ways.
fn format_name(format: Option<image::ImageFormat>) -> &'static str {
    match format {
        None => "same as source",
        Some(image::ImageFormat::Png) => "PNG",
        Some(image::ImageFormat::Jpeg) => "JPEG",
        Some(image::ImageFormat::Gif) => "GIF",
        Some(image::ImageFormat::Bmp) => "BMP",
        Some(image::ImageFormat::Tiff) => "TIFF",
        Some(image::ImageFormat::Ico) => "ICO",
        Some(image::ImageFormat::Tga) => "TGA",
        Some(image::ImageFormat::WebP) => "WebP",
        Some(_) => "?",
    }
}

/// The number in a field, or zero when there is none.
fn number(text: &str) -> u32 {
    text.trim().parse::<u32>().unwrap_or(0)
}

/// A field's text and its unit, as an [`Amount`].
fn amount_of(text: &str, pixels: bool) -> Amount {
    let value = number(text);
    if pixels {
        Amount::Pixels(value)
    } else {
        Amount::Percent(value)
    }
}

/// An [`Amount`]'s number, for the field it opens in.
fn amount_text(amount: Amount) -> String {
    match amount {
        Amount::Percent(value) | Amount::Pixels(value) => value.to_string(),
    }
}

/// The header line: what is being worked on.
const ROW_HEADER: u16 = 0;
/// The keep-ratio checkbox.
const ROW_KEEP: u16 = 1;
/// The width, its field and its unit.
const ROW_WIDTH: u16 = 2;
/// The height, the same way.
const ROW_HEIGHT: u16 = 3;
/// Fit, format and quality, side by side.
const ROW_STEPPERS: u16 = 4;
/// The prefix checkbox and its field.
const ROW_PREFIX: u16 = 5;
/// The postfix checkbox and its field.
const ROW_POSTFIX: u16 = 6;
/// The name the first output will have.
const ROW_PREVIEW: u16 = 7;
/// A refusal, when there is one.
const ROW_ERROR: u16 = 8;
/// `OK` and `Cancel`, or the last row when the box is shorter than that.
const ROW_BUTTONS: u16 = 9;

/// How wide the checkbox half of a name row is.
///
/// Wide enough for the longer of the two labels plus its mark, so the two
/// fields line up under each other rather than stepping in and out.
const NAME_BOX_WIDTH: u16 = 24;

/// The prefix checkbox's label, which says what it changes.
const PREFIX_LABEL: &str = "prefix filename";

/// The postfix checkbox's label.
const POSTFIX_LABEL: &str = "append to filename";

/// Split a name row into its checkbox and its field.
fn name_row(rect: Rect) -> (Rect, Rect) {
    let box_w = NAME_BOX_WIDTH.min(rect.width);
    let boxed = Rect::new(rect.x, rect.y, box_w, 1);
    let field = Rect::new(
        rect.x.saturating_add(box_w),
        rect.y,
        rect.width.saturating_sub(box_w),
        1,
    );
    (boxed, field)
}

/// Split one row into a label, a field and whatever is left for a stepper.
fn label_field_rest(rect: Rect, label_w: u16, field_w: u16) -> (Rect, Rect, Rect) {
    let label = Rect::new(rect.x, rect.y, label_w.min(rect.width), 1);
    let field_x = rect.x.saturating_add(label.width);
    let field = Rect::new(
        field_x,
        rect.y,
        field_w.min(rect.width.saturating_sub(label.width)),
        1,
    );
    let rest_x = field_x.saturating_add(field.width).saturating_add(2);
    let used = label.width.saturating_add(field.width).saturating_add(2);
    let rest = Rect::new(rest_x, rect.y, rect.width.saturating_sub(used), 1);
    (label, field, rest)
}

impl Accelerated for ResizeDialog {
    type Control = Control;

    fn mnemonics(&self) -> &'static [(Control, char)] {
        MNEMONICS
    }

    fn accel(&self, control: Control) -> Accel<Control> {
        match control {
            // A checkbox that gates the two controls under it, so the letter
            // ticks it and leaves the focus where the next thing to say is.
            Control::KeepRatio => Accel::Check,
            // A checkbox that gates a field: tick it and put the caret in the
            // field, which is the whole gesture in one keystroke.
            Control::PrefixOn => Accel::Gate(Control::Prefix),
            Control::PostfixOn => Accel::Gate(Control::Postfix),
            // Every stepper is focus-only: accelerating a many-way control has
            // no meaning that does not turn something off, and `Left` and
            // `Right` are one keystroke away once the focus is on it.
            Control::Width
            | Control::WidthUnit
            | Control::Height
            | Control::HeightUnit
            | Control::Fit
            | Control::Format
            | Control::Quality
            | Control::Prefix
            | Control::Postfix => Accel::Focus,
            Control::Ok | Control::Cancel => Accel::Press,
        }
    }

    fn focus_control(&mut self, control: Control) {
        if let Some(at) = CONTROLS.iter().position(|c| *c == control) {
            self.ring.set(at);
        }
    }

    /// **On**, never a toggle: a mnemonic never turns anything off. `Space` is
    /// how a box comes off, with the focus now on it.
    fn switch_on(&mut self, control: Control) {
        match control {
            Control::KeepRatio => self.keep_ratio = true,
            Control::PrefixOn => self.prefix_on = true,
            Control::PostfixOn => self.postfix_on = true,
            _ => {}
        }
    }

    fn press(&mut self, control: Control) -> DialogOutcome {
        match control {
            Control::Ok => self.accept(),
            Control::Cancel => DialogOutcome::Cancel,
            _ => DialogOutcome::Consumed,
        }
    }
}

impl Dialog for ResizeDialog {
    fn id(&self) -> DialogId {
        DialogId::Resize
    }

    fn title(&self) -> String {
        "Resize images".to_string()
    }

    fn size_hint(&self) -> (u16, u16) {
        // The header, the checkbox, two size rows, the three steppers, the two
        // name rows, the preview, an error line, the buttons, two borders.
        (72, 12)
    }

    /// All twelve letters.
    fn mnemonic_letters(&self) -> Vec<char> {
        self.mnemonics().iter().map(|(_, letter)| *letter).collect()
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        // First, before anything reads `key.action` or reaches a field.
        if let Some(outcome) = self.mnemonic_key(key) {
            return outcome;
        }
        if self.ring.handle(key) {
            return DialogOutcome::Consumed;
        }
        if key.is_cancel() {
            return DialogOutcome::Cancel;
        }
        if key.is_accept() {
            return match self.focused() {
                Control::Cancel => DialogOutcome::Cancel,
                _ => self.accept(),
            };
        }
        let focused = self.focused();
        // The height and its unit are greyed while the ratio is kept, and a
        // greyed control refuses in place rather than accepting an edit that
        // will not be read: with the ratio kept there is one dimension.
        if self.keep_ratio && matches!(focused, Control::Height | Control::HeightUnit) {
            self.error =
                Some("with the ratio kept there is one dimension, and it is the width".to_string());
            return DialogOutcome::Consumed;
        }
        if matches!(focused, Control::Width | Control::Height)
            && key.text().is_some_and(|c| !c.is_ascii_digit())
        {
            // A size is a number. Swallowed rather than ignored, because a
            // dialog consumes all input and a letter that fell through to the
            // panel behind it would move a cursor nobody can see.
            return DialogOutcome::Consumed;
        }
        if let Some(field) = self.field_mut(focused)
            && field.handle(key)
        {
            // Typing a prefix is asking for a prefix, so the box that says so
            // ticks itself. It is never the other way round: nothing here
            // unticks a box, which is the rule an accelerator lives under too.
            match focused {
                Control::Prefix => self.prefix_on = true,
                Control::Postfix => self.postfix_on = true,
                _ => {}
            }
            self.error = None;
            return DialogOutcome::Consumed;
        }
        if key.press.code == KeyCode::Char(' ') {
            match focused {
                Control::KeepRatio => self.keep_ratio = !self.keep_ratio,
                Control::PrefixOn => self.prefix_on = !self.prefix_on,
                Control::PostfixOn => self.postfix_on = !self.postfix_on,
                _ => return self.arrows(key),
            }
            self.error = None;
            return DialogOutcome::Consumed;
        }
        self.arrows(key)
    }

    fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let focused = self.focused();
        let body = style.body();
        let dim = body.add_modifier(Modifier::DIM);
        if let Some(rect) = row(area, ROW_HEADER) {
            draw_text(
                f,
                rect,
                &self.header(),
                style.focus_label(false),
                style.ascii,
            );
        }
        if let Some(rect) = row(area, ROW_KEEP) {
            let text = checkbox("Keep aspect ratio", self.keep_ratio, style.ascii);
            draw_mnemonic(
                f,
                rect,
                &text,
                'k',
                style.focus_label(focused == Control::KeepRatio),
                style.ascii,
            );
        }
        for (index, (label, letter, control, unit, pixels)) in [
            (
                "Width:",
                'w',
                Control::Width,
                Control::WidthUnit,
                self.width_pixels,
            ),
            (
                "Height:",
                'g',
                Control::Height,
                Control::HeightUnit,
                self.height_pixels,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let Some(rect) = row(
                area,
                ROW_WIDTH.saturating_add(u16::try_from(index).unwrap_or(0)),
            ) else {
                continue;
            };
            // The height is greyed while the ratio is kept, for the same
            // reason the fit mode is: there is one dimension there, and it is
            // the one above.
            let live = control == Control::Width || !self.keep_ratio;
            let (label_rect, field_rect, rest) = label_field_rest(rect, 8, 12);
            draw_mnemonic(
                f,
                label_rect,
                label,
                letter,
                if live {
                    style.focus_label(focused == control)
                } else {
                    dim
                },
                style.ascii,
            );
            if live {
                match control {
                    Control::Height => self.height.render(f, field_rect, style),
                    _ => self.width.render(f, field_rect, style),
                }
            } else {
                // Still a field, visibly not this one's turn: bare dim text
                // loses the box that says a value can be typed here at all.
                self.height.render_disabled(f, field_rect, style);
            }
            let unit_style = if live {
                style.button(focused == unit)
            } else {
                dim
            };
            let pieces = [Piece::new(
                Self::unit_label(pixels),
                Some(if control == Control::Width { 'e' } else { 'p' }),
                unit_style,
                live && focused == unit,
            )];
            draw_mnemonic_pieces(f, rest, &pieces, body);
        }
        if let Some(rect) = row(area, ROW_STEPPERS) {
            // Three pieces and not one string: a mnemonic is scoped to its own
            // control's label, and as one string the `f` of `Format` would be
            // found in `Fit`'s half first.
            let fit_style = if self.keep_ratio {
                dim
            } else {
                style.button(focused == Control::Fit)
            };
            let quality_style = if self.has_quality() {
                style.button(focused == Control::Quality)
            } else {
                dim
            };
            let pieces = [
                Piece::new(
                    self.fit_label(),
                    Some('i'),
                    fit_style,
                    !self.keep_ratio && focused == Control::Fit,
                ),
                Piece::new(
                    self.format_label(),
                    Some('f'),
                    style.button(focused == Control::Format),
                    focused == Control::Format,
                ),
                Piece::new(
                    self.quality_label(),
                    Some('q'),
                    quality_style,
                    self.has_quality() && focused == Control::Quality,
                ),
            ];
            draw_mnemonic_pieces(f, rect, &pieces, body);
        }
        for (index, (label, letter, box_control, on, field)) in [
            (
                PREFIX_LABEL,
                'x',
                Control::PrefixOn,
                self.prefix_on,
                &self.prefix,
            ),
            (
                POSTFIX_LABEL,
                'a',
                Control::PostfixOn,
                self.postfix_on,
                &self.postfix,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let Some(rect) = row(
                area,
                ROW_PREFIX.saturating_add(u16::try_from(index).unwrap_or(0)),
            ) else {
                continue;
            };
            let (box_rect, field_rect) = name_row(rect);
            let text = checkbox(label, on, style.ascii);
            draw_mnemonic(
                f,
                box_rect,
                &text,
                letter,
                style.focus_label(focused == box_control),
                style.ascii,
            );
            if on {
                field.render(f, field_rect, style);
            } else {
                // Greyed until its box is ticked, which is what makes the box
                // rather than the field the answer. Drawn as an empty field
                // rather than as nothing, so an unticked row still shows there
                // is a name to type.
                field.render_disabled(f, field_rect, style);
            }
        }
        if let Some(rect) = row(area, ROW_PREVIEW) {
            draw_text(
                f,
                rect,
                &self.preview(),
                style.focus_label(false),
                style.ascii,
            );
        }
        if let Some(rect) = row(area, ROW_ERROR)
            && let Some(error) = self.error.as_deref()
        {
            draw_text(f, rect, error, style.button(true), style.ascii);
        }
        let last = area.height.saturating_sub(1);
        if let Some(rect) = row(area, last.max(ROW_BUTTONS)) {
            let index = match focused {
                Control::Ok => 0,
                Control::Cancel => 1,
                // Focus is on a field, a checkbox or a stepper, so neither
                // button is highlighted.
                _ => usize::MAX,
            };
            draw_mnemonic_buttons(
                f,
                rect,
                &[("OK", Some('o')), ("Cancel", Some('n'))],
                index,
                style,
            );
        }
    }

    fn cursor(&self, area: Rect) -> Option<(u16, u16)> {
        let (field, rect) = match self.focused() {
            Control::Width => (
                &self.width,
                label_field_rest(row(area, ROW_WIDTH)?, 8, 12).1,
            ),
            Control::Height if !self.keep_ratio => (
                &self.height,
                label_field_rest(row(area, ROW_HEIGHT)?, 8, 12).1,
            ),
            Control::Prefix if self.prefix_on => (&self.prefix, name_row(row(area, ROW_PREFIX)?).1),
            Control::Postfix if self.postfix_on => {
                (&self.postfix, name_row(row(area, ROW_POSTFIX)?).1)
            }
            _ => return None,
        };
        field.cursor(rect)
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
#[path = "resize_tests.rs"]
mod tests;
