//! The end-of-batch result summary.
//!
//! > **Errors** never abort the whole batch silently: collect per-file failures
//! > and show a summary at the end with the option to retry the failures.
//!
//! ```text
//! ┌ Copy - 3 failed ─────────────────────────────────────┐
//! │ copied 197 files, 3 dirs; 3 failed                   │
//! │ …/media/10 - POWER PANEL.3mf                         │
//! │   Permission denied (os error 13)                    │
//! │ …/media/notes.txt                                    │
//! │   No space left on device (os error 28)              │
//! │        [ Retry failures ]      [ Close ]             │
//! └──────────────────────────────────────────────────────┘
//! ```
//!
//! Two lines per failure, not one: a path and its error crammed onto one row of
//! a 60-column dialog leaves room for neither, and the error is the half a
//! truncation would eat.
//!
//! # Retry
//!
//! retry needs a [`crate::ops::JobSpec`] rebuilt
//! from [`JobSummary::failures`], which nothing in `ops` does yet. The button is
//! here and answers [`JobAction::Retry`]; the arm that acts on it is the
//! integration point, and it is listed in this milestone's needs.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;

use super::{JobAction, crop_left, row};
use crate::dialog::{
    Accel, Accelerated, Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle, FocusRing,
    draw_mnemonic_buttons, draw_text,
};
use crate::input::{DialogId, KeyCode};
use crate::ops::{JobId, JobSummary};

/// The `Retry failures` button.
const RETRY: usize = 0;
/// The `Close` button.
const CLOSE: usize = 1;

/// the `Alt` mnemonics for this dialog.
///
/// `c` is the program-wide `Close`. `r` is still declared on a clean job, where
/// the retry button is not offered: [`SummaryDialog::accel`] answers
/// [`Accel::Absent`] for it, so the key is swallowed and nothing happens rather
/// than leaking out of a dialog that consumes all input.
pub const MNEMONICS: &[(usize, char)] = &[(RETRY, 'r'), (CLOSE, 'c')];

/// the failure summary.
#[derive(Debug)]
pub struct SummaryDialog {
    id: JobId,
    summary: JobSummary,
    /// First visible failure, for a batch that failed more times than the box
    /// has rows.
    scroll: usize,
    ring: FocusRing,
}

impl SummaryDialog {
    /// A summary of one finished job.
    ///
    /// A clean job has no failures and gets one button; the dialog still opens,
    /// because "it worked" is an answer too and the design asks for a
    /// summary at the end rather than only on failure.
    pub fn new(id: JobId, summary: JobSummary) -> Self {
        let buttons = if summary.failures.is_empty() { 1 } else { 2 };
        let mut ring = FocusRing::new(buttons);
        // Focus `Close`: the `Enter` someone is already pressing must not
        // restart an operation (the reasoning).
        ring.set(buttons.saturating_sub(1));
        Self {
            id,
            summary,
            scroll: 0,
            ring,
        }
    }

    /// The job this summarises.
    pub const fn job(&self) -> JobId {
        self.id
    }

    /// The summary being shown.
    pub const fn summary(&self) -> &JobSummary {
        &self.summary
    }

    /// How many failures there were.
    pub fn failure_count(&self) -> usize {
        self.summary.failures.len()
    }

    /// The one-line headline: what the job did, and what went wrong.
    pub fn headline(&self) -> String {
        self.summary.message()
    }

    /// Two lines per failure: the path, then the error indented under it.
    pub fn failure_lines(&self, width: usize, ascii: bool) -> Vec<String> {
        let mut out = Vec::with_capacity(self.summary.failures.len().saturating_mul(2));
        for failure in &self.summary.failures {
            out.push(crop_left(&failure.path.to_string(), width, ascii));
            let indent = "  ";
            out.push(format!(
                "{indent}{}",
                crop_left(&failure.error, width.saturating_sub(indent.len()), ascii)
            ));
        }
        out
    }

    /// Whether the retry button is offered at all.
    ///
    /// Failures alone are not enough: a paired job's retry cannot be built
    /// from a subset of its sources, so [`crate::ops::JobKind::is_retryable`] gates the
    /// button as well. Offering it for a multi-rename queued a job with no
    /// targets, which renamed nothing and then reported a clean run.
    ///
    pub fn can_retry(&self) -> bool {
        !self.summary.failures.is_empty() && self.summary.kind.is_retryable()
    }

    /// The button labels, each with the letter the design underlines in it.
    fn labels(&self) -> Vec<(&'static str, Option<char>)> {
        if self.can_retry() {
            vec![("Retry failures", Some('r')), ("Close", Some('c'))]
        } else {
            vec![("Close", Some('c'))]
        }
    }

    /// Retry, in one place, for `Enter` and for `Alt+R` alike.
    fn retry(&self) -> DialogOutcome {
        DialogOutcome::Accept(DialogResult::Text(JobAction::Retry(self.id).encode()))
    }

    /// Which button the ring's index means, given that a clean job has one.
    fn focused(&self) -> usize {
        if self.can_retry() {
            self.ring.index()
        } else {
            CLOSE
        }
    }

    /// Scroll the failure list.
    fn scroll_by(&mut self, delta: isize, page: usize) {
        let lines = self.summary.failures.len().saturating_mul(2);
        let last = lines.saturating_sub(page.max(1));
        self.scroll = if delta < 0 {
            self.scroll.saturating_sub(delta.unsigned_abs())
        } else {
            self.scroll.saturating_add(delta.unsigned_abs()).min(last)
        };
    }
}

impl Accelerated for SummaryDialog {
    /// Ring indices rather than an enum: two controls is under
    /// the five-control floor.
    type Control = usize;

    fn mnemonics(&self) -> &'static [(usize, char)] {
        MNEMONICS
    }

    fn accel(&self, control: usize) -> Accel<usize> {
        match control {
            // A job that cannot be retried does not draw the button, so its
            // letter names a control that is not on the screen.
            //
            RETRY if !self.can_retry() => Accel::Absent,
            RETRY | CLOSE => Accel::Press,
            _ => Accel::Focus,
        }
    }

    fn focus_control(&mut self, control: usize) {
        self.ring.set(control);
    }

    /// Nothing here is a checkbox.
    fn switch_on(&mut self, _control: usize) {}

    fn press(&mut self, control: usize) -> DialogOutcome {
        match control {
            RETRY if self.can_retry() => self.retry(),
            // `Close`, and a retry that is not on offer: both leave the summary
            // without starting anything.
            RETRY | CLOSE => DialogOutcome::Cancel,
            _ => DialogOutcome::Consumed,
        }
    }
}

impl Dialog for SummaryDialog {
    fn id(&self) -> DialogId {
        DialogId::JobSummary
    }

    fn title(&self) -> String {
        let failed = self.failure_count();
        if failed == 0 {
            self.summary.kind.title().to_string()
        } else {
            format!("{} - {failed} failed", self.summary.kind.title())
        }
    }

    fn size_hint(&self) -> (u16, u16) {
        let lines =
            u16::try_from(self.summary.failures.len().saturating_mul(2)).unwrap_or(u16::MAX);
        // Headline, the failures, the button row, two borders.
        (68, lines.saturating_add(4).clamp(5, 22))
    }

    /// `Alt+C`, plus `Alt+R` when there is something to retry.
    ///
    ///
    /// Per-instance, which is why [`Dialog::mnemonic_letters`] returns a `Vec`:
    /// a clean job offers one letter and a failed one offers two.
    fn mnemonic_letters(&self) -> Vec<char> {
        self.mnemonics()
            .iter()
            .filter(|(control, _)| match self.accel(*control) {
                Accel::Absent => false,
                Accel::Focus | Accel::Check | Accel::Gate(_) | Accel::Press => true,
            })
            .map(|(_, letter)| *letter)
            .collect()
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        // before anything that reads `key.action`.
        //
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
            // One route to each button: `Enter`
            // presses whichever has focus, exactly as `Alt`+letter does.
            let focused = self.focused();
            return self.press(focused);
        }
        match key.press.code {
            KeyCode::Up => {
                self.scroll_by(-1, 1);
                DialogOutcome::Consumed
            }
            KeyCode::Down => {
                self.scroll_by(1, 1);
                DialogOutcome::Consumed
            }
            KeyCode::PageUp => {
                self.scroll_by(-8, 8);
                DialogOutcome::Consumed
            }
            KeyCode::PageDown => {
                self.scroll_by(8, 8);
                DialogOutcome::Consumed
            }
            KeyCode::Home => {
                self.scroll = 0;
                DialogOutcome::Consumed
            }
            KeyCode::Left => {
                self.ring.prev();
                DialogOutcome::Consumed
            }
            KeyCode::Right => {
                self.ring.next();
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
        if let Some(rect) = row(area, 0) {
            draw_text(f, rect, &self.headline(), body, style.ascii);
        }
        // The button row is the last row; the failures fill what is between.
        let buttons_y = area.height.saturating_sub(1);
        let lines = self.failure_lines(usize::from(area.width), style.ascii);
        let mut index = self.scroll;
        let mut y = 1u16;
        while y < buttons_y {
            let Some(rect) = row(area, y) else { break };
            let Some(line) = lines.get(index) else { break };
            draw_text(f, rect, line, body, style.ascii);
            index = index.saturating_add(1);
            y = y.saturating_add(1);
        }
        if let Some(rect) = row(area, buttons_y).filter(|_| buttons_y > 0) {
            draw_mnemonic_buttons(f, rect, &self.labels(), self.focused(), style);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ColorDepth, Theme};
    use crate::input::KeyPress;
    use crate::ops::{JobFailure, JobKind};
    use crate::vfs::VfsPath;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use std::time::Duration;

    fn key(code: KeyCode) -> DialogKey {
        DialogKey::raw(KeyPress::plain(code))
    }

    fn summary(failures: usize) -> JobSummary {
        JobSummary {
            kind: JobKind::Copy,
            files_done: 197,
            dirs_done: 3,
            bytes_done: 1_000,
            skipped: 0,
            failures: (0..failures)
                .map(|i| JobFailure {
                    path: VfsPath::local(format!("/srv/media/deep/tree/file-{i}.txt")),
                    error: "Permission denied (os error 13)".to_string(),
                })
                .collect(),
            cancelled: false,
            elapsed: Duration::from_secs(12),
            sized: Vec::new(),
            differing: Vec::new(),
        }
    }

    fn dialog(failures: usize) -> SummaryDialog {
        SummaryDialog::new(JobId(4), summary(failures))
    }

    fn render(d: &SummaryDialog, w: u16, h: u16, ascii: bool) -> Buffer {
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

    /// Every character drawn with [`Modifier::UNDERLINED`], in screen order.
    ///
    /// the underline, read off the buffer rather than off the table,
    /// so a declared letter with no paint behind it fails the test that uses
    /// this.
    fn underlined(d: &SummaryDialog, w: u16, h: u16) -> Vec<char> {
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
    fn what_failed_is_listed_per_file_with_its_error() {
        // "collect per-file failures and show a summary".
        let d = dialog(3);
        let out = dump(&render(&d, 80, 24, false));
        assert!(out.contains("file-0.txt"), "{out}");
        assert!(out.contains("Permission denied"), "{out}");
        assert!(out.contains("3 failed"), "the headline:\n{out}");
        assert!(out.contains("Retry failures"), "the retry action:\n{out}");
    }

    #[test]
    fn retrying_names_the_job_whose_failures_are_to_be_retried() {
        let mut d = dialog(2);
        d.handle_key(&key(KeyCode::Left));
        assert_eq!(d.focused(), RETRY);
        match d.handle_key(&key(KeyCode::Enter)) {
            DialogOutcome::Accept(DialogResult::Text(text)) => {
                assert_eq!(JobAction::parse(&text), Some(JobAction::Retry(JobId(4))));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn it_opens_on_close_so_a_stray_enter_does_not_restart_the_operation() {
        let mut d = dialog(2);
        assert_eq!(d.focused(), CLOSE);
        assert!(matches!(
            d.handle_key(&key(KeyCode::Enter)),
            DialogOutcome::Cancel
        ));
    }

    #[test]
    fn a_clean_job_has_no_retry_button_at_all() {
        let d = dialog(0);
        assert!(!d.can_retry());
        assert_eq!(d.labels(), vec![("Close", Some('c'))]);
        let out = dump(&render(&d, 80, 24, false));
        assert!(!out.contains("Retry"), "{out}");
        assert!(out.contains("copied 197 files"), "{out}");
    }

    #[test]
    fn a_long_failure_list_scrolls_rather_than_overflowing() {
        let mut d = dialog(50);
        let before = d.scroll;
        d.handle_key(&key(KeyCode::PageDown));
        assert!(d.scroll > before);
        d.handle_key(&key(KeyCode::Home));
        assert_eq!(d.scroll, 0);
        d.handle_key(&key(KeyCode::Up));
        assert_eq!(d.scroll, 0, "and it never scrolls above the first line");
        // Whatever the scroll, drawing stays inside the box.
        for _ in 0..40 {
            d.handle_key(&key(KeyCode::PageDown));
        }
        let out = dump(&render(&d, 60, 15, false));
        assert_eq!(out.lines().count(), 15, "{out}");
    }

    #[test]
    fn it_renders_at_every_size_the_spec_names() {
        for (w, h) in [(200u16, 50u16), (80, 24), (60, 15)] {
            for ascii in [false, true] {
                let out = dump(&render(&dialog(4), w, h, ascii));
                assert!(out.contains("Close"), "{w}x{h} ascii={ascii}:\n{out}");
            }
        }
    }

    #[test]
    fn every_glyph_it_draws_has_an_ascii_form() {
        for (w, h) in [(200u16, 50u16), (80, 24), (60, 15), (20, 3)] {
            let inner = dump(&render_inner(&dialog(4), w, h, true));
            assert!(inner.is_ascii(), "{w}x{h}:\n{inner}");
        }
    }

    #[test]
    fn a_path_too_long_for_the_box_keeps_its_filename() {
        let d = dialog(1);
        let lines = d.failure_lines(24, false);
        let first = lines.first().cloned().unwrap_or_default();
        assert!(first.ends_with("file-0.txt"), "{first:?}");
        assert!(crate::ui::text::width(&first) <= 24, "{first:?}");
    }

    #[test]
    fn a_multi_rename_is_not_offered_a_retry() {
        // the "option to retry the failures" cannot be built for a
        // paired job: `JobSpec::targets` is positional, so a retry over the
        // subset of sources that failed would have to carry exactly their
        // targets. Dropping them instead queued a `JobKind::Rename` with no
        // targets at all, which renamed nothing and reported "renamed 0 files,
        // 0 dirs" as a clean run - the button said `retrying 1 failed item`
        // and nothing on disk changed.
        //
        // the design gives a multi-rename `Undo` and a result list instead,
        // so there is nothing missing here to add later.
        let mut renamed = summary(2);
        renamed.kind = JobKind::Rename;
        let rename_dialog = SummaryDialog::new(JobId(4), renamed);
        assert!(!rename_dialog.can_retry(), "no retry for a paired job");
        assert_eq!(rename_dialog.labels(), vec![("Close", Some('c'))]);
        let screen = render(&rename_dialog, 80, 24, true);
        let text: String = screen
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(!text.contains("Retry"), "and the button is not drawn");

        // Every other kind still gets it: this is one rule about pairing, not
        // a new rule about failures.
        assert!(dialog(2).can_retry());
        assert!(!JobKind::Rename.is_retryable());
        for kind in [
            JobKind::Copy,
            JobKind::Move,
            JobKind::Delete { trash: true },
            JobKind::Delete { trash: false },
            JobKind::Mkdir,
            JobKind::Size,
        ] {
            assert!(kind.is_retryable(), "{kind:?}");
        }
    }

    #[test]
    fn both_buttons_are_reachable_by_their_alt_letter() {
        // the design on a button: focus it and press it.
        //
        let mut d = dialog(3);
        match d.handle_key(&alt('r')) {
            DialogOutcome::Accept(DialogResult::Text(text)) => {
                assert_eq!(JobAction::parse(&text), Some(JobAction::Retry(JobId(4))));
            }
            other => panic!("Alt+R pressed Retry failures, got {other:?}"),
        }

        let mut d = dialog(3);
        assert!(matches!(d.handle_key(&alt('c')), DialogOutcome::Cancel));
    }

    #[test]
    fn an_absent_control_swallows_its_letter() {
        // a clean job draws no retry
        // button, so `Alt+R` names a control that is not on the screen. The key
        // is consumed, because a dialog consumes all input, and
        // nothing at all happens - it does not fall through to `Close`.
        let mut d = dialog(0);
        assert!(!d.can_retry());
        assert!(matches!(d.handle_key(&alt('r')), DialogOutcome::Consumed));
        assert_eq!(d.focused(), CLOSE, "focus did not move onto a dead button");
        assert_eq!(d.mnemonic_letters(), vec!['c'], "and it is not advertised");

        // With failures to retry, the same letter is live.
        assert_eq!(dialog(3).mnemonic_letters(), vec!['r', 'c']);
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
    }

    #[test]
    fn every_mnemonic_is_underlined_in_its_own_label() {
        // Both states, because this dialog's button row changes with the job
        // (the design I3).
        for failures in [0usize, 3] {
            let d = dialog(failures);
            let mut want = d.mnemonic_letters();
            let mut got = underlined(&d, 80, 24);
            want.sort_unstable();
            got.sort_unstable();
            assert_eq!(got, want, "{failures} failures: underlines on screen");
        }
    }
}
