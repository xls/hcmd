//! The progress dialog.
//!
//! ```text
//! ┌ Copying ─────────────────────────────────────────┐
//! │ ██████████████████░░░░░░░░░░░░░░░░░░░░░░░  43 %  │  current file
//! │ …/Arcade/Leap/stl/10 - POWER PANEL.3mf           │  what it is on
//! │ ████████████░░░░░░░░░░░░░░░░░░░░░░░░░░░░░  29 %  │  whole batch
//! │ 12.4 M/s      10 / 200 files      2.1 G / 8.7 G  │
//! │ 00:34 elapsed, about 01:29 remaining             │
//! │              [ F2 Background ]   [ Esc Cancel ]  │
//! └──────────────────────────────────────────────────┘
//! ```
//!
//! # The dialog is a *view* of a job
//!
//! [`JobStatus`] is the model. The dialog holds a
//! snapshot of it and [`ProgressDialog::update`] replaces that snapshot as
//! [`crate::ops::JobUpdate`]s arrive, which is also what makes backgrounding
//! reversible - nothing about the worker changes, and a job brought forward
//! gets "the same bars, same counts, same rate, still running".
//!
//! # Two buttons, three accelerators, and none of them is the real one
//!
//! the design is explicit: `Background` and `Cancel` are in the focus ring
//! *and* `F2` and `Esc` work without focusing them. the design adds a third
//! route, `Alt+B` and `Alt+N`, because a user who has learned that `Alt+N` is
//! `Cancel` everywhere else should not find it dead here.
//!
//!
//! **All three go through [`crate::dialog::Accelerated::press`]**, which is the
//! one place either button acts, so "neither route is the real one" is true by
//! construction rather than by three copies staying in step. Each answers with
//! a [`super::JobAction`], because [`crate::dialog::DialogResult`] has no
//! job-shaped variant - including `Esc`, which must **answer** rather than
//! merely close: a dialog that pops without cancelling leaves a worker running
//! with nothing watching it.
//!
//! # Smoothing ("must not flicker wildly")
//!
//! [`crate::ops::RateMeter`] already gives two honest numbers: a windowed rate,
//! so a stall shows as one, and a cumulative ETA, so it does not jump. Both
//! still arrive as a fresh figure ten times a second, and a display that
//! repaints `12.4 M/s`, `9.7 M/s`, `14.1 M/s` in three consecutive frames is
//! unreadable even when every figure is true. [`Smoother`] is the *display*
//! filter: an exponential moving average on the rate, and a quantized,
//! hysteretic ETA that only moves when the change would survive rounding. It
//! never invents a figure - `None` in is `None` out (below
//! `ops.rate_min_samples` the rate is `-` and the ETA is omitted).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use std::time::Duration;

use super::{
    JobAction, MIN_BAR_WIDTH, clock_text, crop_left, draw_bar, draw_split, rate_text, row,
};
use crate::dialog::{
    Accel, Accelerated, Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle, FocusRing,
    draw_mnemonic_buttons, draw_text,
};
use crate::input::{DialogId, KeyCode};
use crate::ops::{JobId, JobKind, JobStatus};
use crate::panel::format::human_size;
use crate::ui::text;

/// How much of the previous displayed rate survives one update.
///
/// 0.7 is roughly a three-sample time constant, which at
/// [`crate::ops::PROGRESS_INTERVAL`] is about a third of a second - long enough
/// to be readable, short enough that a stall is visible well inside the
/// `ops.rate_window` the underlying meter already averages over.
const RATE_INERTIA: f64 = 0.7;

/// Below this the ETA is shown to the second; there is no point rounding
/// "3 seconds left" to the nearest five.
const ETA_FINE: u64 = 60;
/// Below this the ETA is rounded to five seconds.
const ETA_MEDIUM: u64 = 600;

/// The display filter for the rate and the ETA.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Smoother {
    rate: Option<f64>,
    eta: Option<Duration>,
}

impl Smoother {
    /// A filter that has seen nothing yet.
    pub const fn new() -> Self {
        Self {
            rate: None,
            eta: None,
        }
    }

    /// Feed it one sample. `None` clears the corresponding figure: the design
    /// would rather show nothing than a number that is no longer true.
    pub fn push(&mut self, rate: Option<u64>, eta: Option<Duration>) {
        self.rate = match (self.rate, rate) {
            (_, None) => None,
            (None, Some(fresh)) => {
                // A displayed rate, smoothed for readability - the low bits of
                // a byte count per second are noise by the time they are shown.
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "the low bits of a byte count per second are noise once shown"
                )]
                Some(fresh as f64)
            }
            (Some(old), Some(fresh)) => {
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "the low bits of a byte count per second are noise once shown"
                )]
                let fresh = fresh as f64;
                Some(old.mul_add(RATE_INERTIA, fresh * (1.0 - RATE_INERTIA)))
            }
        };
        self.eta = match eta {
            None => None,
            Some(fresh) => Some(match self.eta {
                // Only move when the change survives the rounding the display
                // does anyway; otherwise the figure would twitch between two
                // renderings of the same estimate.
                Some(shown) if quantize_eta(fresh) == quantize_eta(shown) => shown,
                _ => fresh,
            }),
        };
    }

    /// The rate to display, in bytes per second.
    pub fn rate(&self) -> Option<u64> {
        // `max(0.0)` is the sign-loss guard, and a rate large enough to
        // truncate would have to exceed 2^64 bytes per second; a float-to-int
        // cast saturates in Rust either way, so neither can panic.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "max(0.0) guards the sign and a float-to-int cast saturates"
        )]
        self.rate.map(|r| r.max(0.0).round() as u64)
    }

    /// The remaining time to display, already rounded to a granularity that
    /// cannot flicker.
    pub fn eta(&self) -> Option<Duration> {
        self.eta.map(quantize_eta)
    }
}

/// Round a remaining time to a granularity coarse enough not to twitch:
/// seconds under a minute, five seconds under ten minutes, half a minute above.
fn quantize_eta(d: Duration) -> Duration {
    let secs = d.as_secs();
    let step: u64 = if secs < ETA_FINE {
        1
    } else if secs < ETA_MEDIUM {
        5
    } else {
        30
    };
    let rounded = secs
        .checked_add(step.checked_div(2).unwrap_or(0))
        .unwrap_or(secs)
        .checked_div(step)
        .unwrap_or(secs)
        .saturating_mul(step);
    Duration::from_secs(rounded.max(step.min(secs.max(1))))
}

/// Which row of the interior a piece of the dialog occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// The current file's bar.
    FileBar,
    /// The current file's path.
    File,
    /// The whole batch's bar.
    BatchBar,
    /// Rate, files done / total, bytes done / total.
    Counts,
    /// Elapsed and remaining.
    Times,
    /// `F2 Background` and `Esc Cancel`.
    Buttons,
}

/// "the elapsed/remaining line is the first thing dropped when
/// there is no room". Then the bars, which are decoration over the counts;
/// the counts and the file name are the answer to "what is it doing".
const DROP_ORDER: [Slot; 4] = [Slot::Times, Slot::BatchBar, Slot::FileBar, Slot::Counts];

/// The `Background` button.
const BACKGROUND: usize = 0;
/// The `Cancel` button.
const CANCEL: usize = 1;
/// The two buttons, in ring order, with their accelerators in the label -
/// the design wants both routes visible, and a footer that only prints the
/// shortcut leaves someone looking for something to press. Each carries the
/// `Alt` mnemonic the design underlines in it, so a label and its underline
/// are declared in one place.
const BUTTONS: [(&str, Option<char>); 2] =
    [("F2 Background", Some('b')), ("Esc Cancel", Some('n'))];

/// the `Alt` mnemonics for this dialog.
///
/// A **third** route to each button, on top of the `F2` and `Esc`. "neither
/// route is the real one"; a third does not change that, and a user who has
/// learned that `Alt+N` is `Cancel` everywhere else should not find it dead
/// here. All three now go through [`Accelerated::press`], so there is one
/// definition of what each button does rather than one per route.
pub const MNEMONICS: &[(usize, char)] = &[(BACKGROUND, 'b'), (CANCEL, 'n')];

/// the progress dialog.
#[derive(Debug)]
pub struct ProgressDialog {
    status: JobStatus,
    /// `ops.file_bar_min_size`: below this the per-file bar is omitted because
    /// it would only ever flash.
    file_bar_min: u64,
    smoother: Smoother,
    ring: FocusRing,
}

impl ProgressDialog {
    /// A dialog viewing `status`. `file_bar_min` is `ops.file_bar_min_size`.
    ///
    /// # Byte counts here are human-readable whatever `panel.human_sizes` says
    ///
    /// the design ties the panel's *status line* to the `size` column so
    /// the two never disagree; this is neither. the own diagram
    /// reads `2.1 G / 8.7 G`, and it is right to: `2,100,000,000 /
    /// 8,700,000,000` is 27 columns of a 52-column dialog spent on digits
    /// nobody reads while a copy is running, and it crowds out the file count
    /// beside it. A progress dialog answers "how much is left", not "exactly
    /// how big is this".
    pub fn new(status: JobStatus, file_bar_min: u64) -> Self {
        let mut smoother = Smoother::new();
        smoother.push(status.throughput, status.eta);
        Self {
            status,
            file_bar_min,
            smoother,
            ring: FocusRing::new(2),
        }
    }

    /// Replace the snapshot as the worker reports.
    pub fn update(&mut self, status: &JobStatus) {
        self.smoother.push(status.throughput, status.eta);
        self.status = status.clone();
    }

    /// The job being watched.
    pub const fn job(&self) -> JobId {
        self.status.id
    }

    /// What it is doing.
    pub const fn kind(&self) -> JobKind {
        self.status.kind
    }

    /// The snapshot currently drawn.
    pub const fn status(&self) -> &JobStatus {
        &self.status
    }

    /// The smoothed rate, in bytes per second.
    pub fn rate(&self) -> Option<u64> {
        self.smoother.rate()
    }

    /// The smoothed, rounded time remaining.
    pub fn eta(&self) -> Option<Duration> {
        self.smoother.eta()
    }

    /// the counts line: rate, files, bytes.
    pub fn counts_line(&self, ascii: bool) -> String {
        let rate = rate_text(self.rate(), ascii);
        let files = if self.status.files_total > 0 {
            format!(
                "{} / {} files",
                self.status.files_done, self.status.files_total
            )
        } else {
            format!("{} files", self.status.files_done)
        };
        format!("{rate}   {files}   {}", self.bytes_line())
    }

    /// `2.1 G / 8.7 G`, or just `2.1 G` when there is no total.
    pub fn bytes_line(&self) -> String {
        if self.status.bytes_total > 0 {
            format!(
                "{} / {}",
                human_size(self.status.bytes_done),
                human_size(self.status.bytes_total)
            )
        } else {
            human_size(self.status.bytes_done)
        }
    }

    /// the elapsed / remaining line.
    pub fn times_line(&self) -> String {
        let elapsed = clock_text(self.status.elapsed);
        match self.eta() {
            Some(eta) => format!("{elapsed} elapsed, about {} remaining", clock_text(eta)),
            None => format!("{elapsed} elapsed"),
        }
    }

    /// Which slots this interior has room for.
    fn rows(&self, area: Rect) -> Vec<(Slot, Rect)> {
        let mut wanted = Vec::with_capacity(6);
        let bars_fit = area.width >= MIN_BAR_WIDTH;
        if bars_fit && self.status.show_file_bar(self.file_bar_min) {
            wanted.push(Slot::FileBar);
        }
        wanted.push(Slot::File);
        if bars_fit && self.status.show_batch_bar() {
            wanted.push(Slot::BatchBar);
        }
        wanted.push(Slot::Counts);
        wanted.push(Slot::Times);
        wanted.push(Slot::Buttons);

        let height = usize::from(area.height);
        for slot in DROP_ORDER {
            if wanted.len() <= height {
                break;
            }
            wanted.retain(|s| *s != slot);
        }
        wanted.truncate(height);

        wanted
            .into_iter()
            .enumerate()
            .filter_map(|(i, slot)| {
                let index = u16::try_from(i).unwrap_or(u16::MAX);
                row(area, index).map(|rect| (slot, rect))
            })
            .collect()
    }

    /// `Esc`, and the `Cancel` button: stop the job.
    fn cancel(&self) -> DialogOutcome {
        DialogOutcome::Accept(DialogResult::Text(
            JobAction::Cancel(self.status.id).encode(),
        ))
    }

    /// `F2`, and the `Background` button.
    fn background(&self) -> DialogOutcome {
        DialogOutcome::Accept(DialogResult::Text(
            JobAction::Background(self.status.id).encode(),
        ))
    }
}

impl Accelerated for ProgressDialog {
    /// Ring indices rather than an enum: two controls is under
    /// the five-control floor.
    type Control = usize;

    fn mnemonics(&self) -> &'static [(usize, char)] {
        MNEMONICS
    }

    fn accel(&self, control: usize) -> Accel<usize> {
        match control {
            BACKGROUND | CANCEL => Accel::Press,
            _ => Accel::Focus,
        }
    }

    fn focus_control(&mut self, control: usize) {
        self.ring.set(control);
    }

    /// Nothing here is a checkbox.
    fn switch_on(&mut self, _control: usize) {}

    /// The one place either button's action lives.
    ///
    /// `F2`, `Esc`, `Enter` on a focused button and `Alt`+letter all arrive
    /// here, so the "neither route is the real one" is true by
    /// construction rather than by two copies staying in step.
    fn press(&mut self, control: usize) -> DialogOutcome {
        match control {
            BACKGROUND => self.background(),
            CANCEL => self.cancel(),
            _ => DialogOutcome::Consumed,
        }
    }
}

impl Dialog for ProgressDialog {
    fn id(&self) -> DialogId {
        DialogId::Progress
    }

    fn title(&self) -> String {
        self.status.kind.title().to_string()
    }

    fn size_hint(&self) -> (u16, u16) {
        // Six interior rows at the widest, and the diagram is 52 columns of
        // interior. `centred` clamps both.
        (56, 8)
    }

    /// Follow this dialog's own job, and ignore every other row.
    ///
    /// A job whose row has been forgotten leaves the last state on screen
    /// rather than blanking: the dialog is about to be closed by
    /// [`crate::app::App::sync_job_dialogs`] anyway, and a frame of zeros in
    /// between would read as the transfer having restarted.
    fn job_update(&mut self, jobs: &[JobStatus]) {
        if let Some(status) = jobs.iter().find(|j| j.id == self.status.id) {
            self.update(status);
        }
    }

    fn job(&self) -> Option<JobId> {
        Some(self.status.id)
    }

    /// `Alt+B` and `Alt+N`.
    fn mnemonic_letters(&self) -> Vec<char> {
        self.mnemonics().iter().map(|(_, letter)| *letter).collect()
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        // before anything that reads `key.action`.
        // `is_cancel` reads
        // `Some(Action::Clear)`, so a user who bound `alt+n` to `clear` must
        // still get this dialog's `Cancel` from it - which is the same button,
        // by the same route.
        if let Some(outcome) = self.mnemonic_key(key) {
            return outcome;
        }
        if self.ring.handle(key) {
            return DialogOutcome::Consumed;
        }
        // each button has an accelerator that works without
        // focusing it, and neither route is the "real" one. Every route goes
        // through `press`, so there is one rule and not three.
        if key.is_cancel() {
            return self.press(CANCEL);
        }
        if key.press.code == KeyCode::F(2) {
            return self.press(BACKGROUND);
        }
        if key.is_accept() {
            return if self.ring.is(BACKGROUND) {
                self.press(BACKGROUND)
            } else {
                self.press(CANCEL)
            };
        }
        // Two buttons side by side: Left is the left one and Right is the
        // right one, rather than a rotation that reads as arbitrary at a count
        // of two.
        match key.press.code {
            KeyCode::Left | KeyCode::Up => {
                self.ring.set(BACKGROUND);
                DialogOutcome::Consumed
            }
            KeyCode::Right | KeyCode::Down => {
                self.ring.set(CANCEL);
                DialogOutcome::Consumed
            }
            _ => DialogOutcome::Ignored,
        }
    }

    fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let body: Style = style.body();
        for (slot, rect) in self.rows(area) {
            match slot {
                Slot::FileBar => draw_bar(f, rect, self.status.file_fraction(), style),
                Slot::File => {
                    let path = crop_left(&self.status.file, usize::from(rect.width), style.ascii);
                    draw_text(f, rect, &path, body, style.ascii);
                }
                Slot::BatchBar => draw_bar(f, rect, self.status.fraction(), style),
                Slot::Counts => {
                    let line = self.counts_line(style.ascii);
                    let fits = text::width(&line) <= usize::from(rect.width);
                    if fits {
                        draw_text(f, rect, &line, body, style.ascii);
                    } else {
                        // Too narrow for the three-column form: the rate on the
                        // left, the byte counts on the right, and the file
                        // count goes - the bars already say how far along it is.
                        draw_split(
                            f,
                            rect,
                            &rate_text(self.rate(), style.ascii),
                            &self.bytes_line(),
                            body,
                            style.ascii,
                        );
                    }
                }
                Slot::Times => draw_text(f, rect, &self.times_line(), body, style.ascii),
                Slot::Buttons => {
                    draw_mnemonic_buttons(f, rect, &BUTTONS, self.ring.index(), style);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ColorDepth, Theme};
    use crate::input::KeyPress;
    use crate::ops::{JobEvent, JobSummary};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    fn key(code: KeyCode) -> DialogKey {
        DialogKey::raw(KeyPress::plain(code))
    }

    fn running() -> JobStatus {
        let mut status = JobStatus::queued(JobId(3), JobKind::Copy);
        status.apply(&JobEvent::Progress {
            file: "/srv/media/Arcade/Leap/stl/10 - POWER PANEL.3mf".to_string(),
            file_bytes_done: 43,
            file_bytes_total: 100,
            files_done: 10,
            files_total: 200,
            bytes_done: 2_100_000_000,
            bytes_total: 8_700_000_000,
            throughput: Some(13_002_342),
            eta: Some(Duration::from_secs(89)),
            elapsed: Duration::from_secs(34),
        });
        // A file bar wants a file big enough to be worth one.
        status.file_bytes_total = 40_000_000;
        status.file_bytes_done = 17_200_000;
        status
    }

    fn dialog() -> ProgressDialog {
        ProgressDialog::new(running(), 1024 * 1024)
    }

    fn render(d: &ProgressDialog, w: u16, h: u16, ascii: bool) -> Buffer {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let st = DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, ascii);
        terminal
            .draw(|f| {
                crate::dialog::draw(f, d, f.area(), &st);
            })
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    /// The dialog's own interior, without the framework's border.
    ///
    /// The frame is `crate::dialog::draw`'s and is tested there and in
    /// `ui::tests::every_v02_dialog_renders_inside_the_minimum_terminal`,
    /// which draws the whole box over the real panels at 60x15 in both glyph
    /// sets. This renders what *this* dialog is responsible for, so an ASCII
    /// failure below is the dialog's own and not the border's.
    fn render_inner(d: &impl Dialog, w: u16, h: u16, ascii: bool) -> Buffer {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let st = DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, ascii);
        terminal
            .draw(|f| {
                let area = f.area();
                d.render(f, area, &st);
            })
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    /// Every character drawn with [`ratatui::style::Modifier::UNDERLINED`], in
    /// screen order, folded to lower case.
    ///
    /// the underline, read off the buffer rather than off the table,
    /// so a declared letter with no paint behind it fails the test that uses
    /// this.
    fn underlined(d: &ProgressDialog, w: u16, h: u16) -> Vec<char> {
        let buffer = render(d, w, h, false);
        let mut out = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let Some(cell) = buffer.cell((x, y)) else {
                    continue;
                };
                if cell.modifier.contains(ratatui::style::Modifier::UNDERLINED) {
                    out.extend(cell.symbol().chars());
                }
            }
        }
        out.iter().map(|c| c.to_ascii_lowercase()).collect()
    }

    /// `Alt`+letter, the keystroke the design is about.
    fn alt(c: char) -> DialogKey {
        DialogKey::raw(KeyPress::new(
            KeyCode::Char(c),
            crate::input::KeyModifiers::ALT,
        ))
    }

    fn dump(buf: &Buffer) -> String {
        let area = buf.area();
        let mut out = String::new();
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                out.push_str(buf.cell((x, y)).map_or(" ", |c| c.symbol()));
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn a_zero_byte_copy_shows_counts_rather_than_a_bar_and_never_a_nan() {
        // A batch with no bytes in it - one empty file - is the division by
        // zero every progress display has waiting for it. the design shows
        // counts either way, so the answer is to draw no bar, not to draw one
        // computed from nothing.
        let mut status = JobStatus::queued(JobId(1), JobKind::Copy);
        status.started = true;
        status.file = "/tmp/empty.txt".to_string();
        status.files_total = 1;
        status.files_done = 1;
        let d = ProgressDialog::new(status, 0);
        for (w, h) in [(200u16, 50u16), (80, 24), (60, 15), (24, 4)] {
            for ascii in [false, true] {
                let out = dump(&render_inner(&d, w, h, ascii));
                assert!(
                    !out.contains("NaN") && !out.contains("inf"),
                    "{w}x{h}: a figure divided by nothing:\n{out}"
                );
                assert!(
                    !out.contains('%'),
                    "{w}x{h}: a percentage of no bytes:\n{out}"
                );
            }
        }
    }

    #[test]
    fn esc_answers_rather_than_merely_closing() {
        // A dialog that pops without cancelling leaves the worker running with
        // nothing watching it.
        let mut d = dialog();
        match d.handle_key(&key(KeyCode::Esc)) {
            DialogOutcome::Accept(DialogResult::Text(text)) => {
                assert_eq!(JobAction::parse(&text), Some(JobAction::Cancel(JobId(3))));
            }
            other => panic!("Esc must cancel the job, got {other:?}"),
        }
    }

    #[test]
    fn both_buttons_are_reachable_by_accelerator_and_by_tab() {
        // "Neither route is the 'real' one."
        let mut d = dialog();
        match d.handle_key(&key(KeyCode::F(2))) {
            DialogOutcome::Accept(DialogResult::Text(text)) => {
                assert_eq!(
                    JobAction::parse(&text),
                    Some(JobAction::Background(JobId(3)))
                );
            }
            other => panic!("F2 must background, got {other:?}"),
        }

        let mut d = dialog();
        assert!(d.ring.is(BACKGROUND));
        match d.handle_key(&key(KeyCode::Enter)) {
            DialogOutcome::Accept(DialogResult::Text(text)) => {
                assert_eq!(
                    JobAction::parse(&text),
                    Some(JobAction::Background(JobId(3)))
                );
            }
            other => panic!("Enter on Background must background, got {other:?}"),
        }

        let mut d = dialog();
        d.handle_key(&key(KeyCode::Tab));
        assert!(d.ring.is(CANCEL));
        match d.handle_key(&key(KeyCode::Enter)) {
            DialogOutcome::Accept(DialogResult::Text(text)) => {
                assert_eq!(JobAction::parse(&text), Some(JobAction::Cancel(JobId(3))));
            }
            other => panic!("Enter on Cancel must cancel, got {other:?}"),
        }
    }

    #[test]
    fn the_rate_is_smoothed_and_never_fabricated() {
        // below the sample floor the rate is `-`, and a stall
        // must still show as one rather than being averaged away.
        let mut s = Smoother::new();
        assert_eq!(s.rate(), None);
        s.push(None, None);
        assert_eq!(s.rate(), None, "None in, None out");

        s.push(Some(10_000_000), Some(Duration::from_secs(100)));
        assert_eq!(
            s.rate(),
            Some(10_000_000),
            "the first sample is taken whole"
        );

        // A single wild sample moves the display by less than it moves.
        s.push(Some(30_000_000), Some(Duration::from_secs(100)));
        let shown = s.rate().unwrap_or(0);
        assert!(shown > 10_000_000 && shown < 20_000_000, "{shown}");

        // But a sustained change gets there.
        for _ in 0..20 {
            s.push(Some(30_000_000), Some(Duration::from_secs(100)));
        }
        let shown = s.rate().unwrap_or(0);
        assert!(shown > 29_000_000, "{shown}");

        // And a stall is visible: the meter reports zero, the display follows.
        for _ in 0..20 {
            s.push(Some(0), Some(Duration::from_secs(100)));
        }
        assert!(s.rate().unwrap_or(u64::MAX) < 1_000_000, "{:?}", s.rate());
    }

    #[test]
    fn the_eta_does_not_twitch_between_two_renderings_of_one_estimate() {
        let mut s = Smoother::new();
        s.push(Some(1), Some(Duration::from_secs(300)));
        let first = s.eta();
        // A jitter of a second on a five-minute estimate must not move it.
        for delta in [299u64, 301, 302, 298] {
            s.push(Some(1), Some(Duration::from_secs(delta)));
            assert_eq!(s.eta(), first, "{delta}s moved a 300s estimate");
        }
        // A real change does move it.
        s.push(Some(1), Some(Duration::from_secs(120)));
        assert_ne!(s.eta(), first);
        // And it can be withdrawn entirely.
        s.push(Some(1), None);
        assert_eq!(s.eta(), None);
    }

    #[test]
    fn the_eta_is_quantized_coarsely_enough_to_read() {
        assert_eq!(quantize_eta(Duration::from_secs(7)), Duration::from_secs(7));
        assert_eq!(
            quantize_eta(Duration::from_secs(97)),
            Duration::from_secs(95)
        );
        assert_eq!(
            quantize_eta(Duration::from_secs(1_000)),
            Duration::from_secs(990)
        );
        // Never rounds a live estimate down to nothing.
        assert!(quantize_eta(Duration::from_secs(1)) >= Duration::from_secs(1));
        assert_eq!(quantize_eta(Duration::ZERO), Duration::from_secs(1));
    }

    #[test]
    fn the_file_path_is_cropped_from_the_left() {
        // twenty files called index.html.
        let out = dump(&render(&dialog(), 60, 15, false));
        assert!(
            out.contains("POWER PANEL.3mf"),
            "the filename must survive:\n{out}"
        );
    }

    #[test]
    fn it_renders_at_every_size_the_spec_names() {
        for (w, h) in [(200u16, 50u16), (80, 24), (60, 15)] {
            for ascii in [false, true] {
                let out = dump(&render(&dialog(), w, h, ascii));
                assert!(out.contains("Cancel"), "{w}x{h} ascii={ascii}:\n{out}");
                assert!(out.contains("Copying"), "{w}x{h}:\n{out}");
            }
        }
    }

    #[test]
    fn every_glyph_it_draws_has_an_ascii_form() {
        // and the bars are the whole reason it matters here.
        for (w, h) in [(200u16, 50u16), (80, 24), (60, 15), (24, 4)] {
            let inner = dump(&render_inner(&dialog(), w, h, true));
            assert!(inner.is_ascii(), "{w}x{h}:\n{inner}");
            assert!(inner.contains('#'), "the filled bar:\n{inner}");
        }
    }

    #[test]
    fn a_small_single_file_has_no_bar_at_all_and_that_is_intended() {
        // below `ops.file_bar_min_size` and a batch of one.
        let mut status = JobStatus::queued(JobId(1), JobKind::Copy);
        status.files_total = 1;
        status.file_bytes_total = 400;
        status.file_bytes_done = 100;
        status.file = "/tmp/a.txt".to_string();
        let d = ProgressDialog::new(status, 1024 * 1024);
        let slots: Vec<Slot> = d
            .rows(Rect::new(0, 0, 50, 6))
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        assert!(!slots.contains(&Slot::FileBar), "{slots:?}");
        assert!(!slots.contains(&Slot::BatchBar), "{slots:?}");
        assert!(slots.contains(&Slot::File), "{slots:?}");
        assert!(slots.contains(&Slot::Counts), "{slots:?}");
    }

    #[test]
    fn the_elapsed_line_is_the_first_thing_dropped() {
        // the design says so in as many words.
        let d = dialog();
        let full: Vec<Slot> = d
            .rows(Rect::new(0, 0, 50, 6))
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        assert!(full.contains(&Slot::Times), "{full:?}");
        let tight: Vec<Slot> = d
            .rows(Rect::new(0, 0, 50, 5))
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        assert!(!tight.contains(&Slot::Times), "{tight:?}");
        assert!(tight.contains(&Slot::Buttons), "{tight:?}");
        assert!(tight.contains(&Slot::File), "{tight:?}");
    }

    #[test]
    fn no_row_ever_leaves_the_interior_and_none_is_zero_sized() {
        let d = dialog();
        for h in 0u16..10 {
            for w in [0u16, 1, 8, 20, 60] {
                let area = Rect::new(3, 2, w, h);
                for (_, rect) in d.rows(area) {
                    assert!(rect.width > 0 && rect.height > 0, "{w}x{h}: {rect:?}");
                    assert!(rect.bottom() <= area.bottom(), "{w}x{h}: {rect:?}");
                    assert!(rect.right() <= area.right(), "{w}x{h}: {rect:?}");
                }
            }
        }
    }

    #[test]
    fn a_size_job_shows_counts_rather_than_a_bar_that_lurches() {
        // a walk has no denominator.
        let mut status = JobStatus::queued(JobId(9), JobKind::Size);
        status.files_done = 4_000;
        status.bytes_done = 900_000;
        status.file = "/srv/media".to_string();
        let d = ProgressDialog::new(status, 1024 * 1024);
        assert_eq!(d.status().fraction(), None);
        let line = d.counts_line(false);
        assert!(line.contains("4000 files"), "{line}");
        assert!(!line.contains('/'), "no denominator to show: {line}");
    }

    #[test]
    fn a_finished_job_still_renders() {
        let mut status = running();
        status.apply(&JobEvent::Finished {
            summary: Box::new(JobSummary {
                kind: JobKind::Copy,
                files_done: 200,
                dirs_done: 3,
                bytes_done: 8_700_000_000,
                skipped: 0,
                failures: Vec::new(),
                cancelled: false,
                elapsed: Duration::from_secs(120),
                sized: Vec::new(),
                differing: Vec::new(),
                first_difference: None,
            }),
        });
        let d = ProgressDialog::new(status, 1024 * 1024);
        let out = dump(&render(&d, 60, 15, false));
        assert!(out.contains("Copying"), "{out}");
    }

    #[test]
    fn both_buttons_answer_to_a_third_route_as_well() {
        // the design gives each button an accelerator in its label and says
        // neither route is the real one; the design adds
        // the letters as a third, because a user who has learned
        // that `Alt+N` is `Cancel` everywhere else should not find it dead
        // here. All three go through `Accelerated::press`.
        let mut d = dialog();
        match d.handle_key(&alt('b')) {
            DialogOutcome::Accept(DialogResult::Text(text)) => assert_eq!(
                JobAction::parse(&text),
                Some(JobAction::Background(JobId(3)))
            ),
            other => panic!("Alt+B backgrounded, got {other:?}"),
        }

        let mut d = dialog();
        match d.handle_key(&alt('n')) {
            DialogOutcome::Accept(DialogResult::Text(text)) => {
                assert_eq!(JobAction::parse(&text), Some(JobAction::Cancel(JobId(3))))
            }
            other => panic!("Alt+N cancelled, got {other:?}"),
        }

        // And the older two routes still answer the same way.
        let mut d = dialog();
        let by_f2 = d.handle_key(&key(KeyCode::F(2)));
        let mut d = dialog();
        let by_letter = d.handle_key(&alt('b'));
        assert_eq!(format!("{by_f2:?}"), format!("{by_letter:?}"));
    }

    #[test]
    fn mnemonics_are_unique_within_this_dialog() {
        // a duplicate is a bug rather than a first-one-wins rule.
        let mut seen: Vec<char> = Vec::new();
        for (control, letter) in MNEMONICS {
            assert!(letter.is_ascii_lowercase(), "{control}: stored folded");
            assert!(!seen.contains(letter), "{control}: Alt+{letter} is taken");
            seen.push(*letter);
        }
        assert_eq!(dialog().mnemonic_letters(), vec!['b', 'n']);
    }

    #[test]
    fn every_mnemonic_is_underlined_in_its_own_label() {
        let d = dialog();
        let mut want = d.mnemonic_letters();
        let mut got = underlined(&d, 80, 24);
        want.sort_unstable();
        got.sort_unstable();
        assert_eq!(got, want, "underlines on screen");
    }
}
