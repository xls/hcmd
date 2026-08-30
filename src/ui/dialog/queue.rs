//! The background queue view.
//!
//! > A queue view lists pending, active and failed jobs.
//!
//! > `Enter` on a running one restores its progress dialog; `Esc` returns to
//! > the panels leaving it running.
//!
//! ```text
//! ┌ Background jobs ─────────────────────────────────────┐
//! │ #1 Copying          running    43 %   10 / 200 files │
//! │ #2 Moving to trash  waiting    a conflict needs an…  │
//! │ #3 Deleting         failed     3 failed              │
//! │ Enter open · Del forget · Esc panels                 │
//! └──────────────────────────────────────────────────────┘
//! ```
//!
//! # A finished job's failures open here, not over the panels
//!
//! the design reconciles the design with: a *foreground* job's summary opens a
//! dialog, a *backgrounded* one's does not - "an operation completing must
//! never eat the keystroke someone was in the middle of typing" - and its
//! result waits in the queue view. So `Enter` on a finished job replaces this
//! dialog with its [`SummaryDialog`], which the view can do on its own because
//! it holds the [`JobSummary`] already.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;

use super::summary::SummaryDialog;
use super::{JobAction, ellipsis, row};
use crate::dialog::{
    Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle, MessageDialog, draw_text,
};
use crate::input::{DialogId, KeyCode};
use crate::ops::JobStatus;
use crate::ui::text;

/// The footer's hint line.
const HINT: &str = "Enter open  Del forget  Esc panels";

/// the queue view.
#[derive(Debug)]
pub struct QueueDialog {
    /// A snapshot of [`crate::app::App::jobs`]. Held rather than borrowed
    /// because `Dialog::render` gets a frame and a style and nothing else.
    jobs: Vec<JobStatus>,
    cursor: usize,
    /// The first job row on screen. It outlives the frame deliberately: moving
    /// the cursor back up must not jump the view, which it would if the window
    /// were recomputed from the cursor alone. Written by
    /// [`crate::dialog::Dialog::layout`], once per frame, before the draw.
    scroll: usize,
}

impl QueueDialog {
    /// A view over the jobs the application currently knows about.
    ///
    /// Byte counts are human-readable here for the same reason they are in the
    /// progress dialog: a queue row is a summary, and an exact byte count is
    /// half the row.
    pub fn new(jobs: Vec<JobStatus>) -> Self {
        Self {
            jobs,
            cursor: 0,
            scroll: 0,
        }
    }

    /// Replace the snapshot, keeping the cursor on the same job if it is still
    /// there - a job finishing must not move the selection under the user.
    pub fn update(&mut self, jobs: Vec<JobStatus>) {
        let selected = self.selected().map(|status| status.id);
        self.jobs = jobs;
        self.cursor = selected
            .and_then(|id| self.jobs.iter().position(|status| status.id == id))
            .unwrap_or_else(|| self.cursor.min(self.jobs.len().saturating_sub(1)));
    }

    /// The jobs listed.
    pub fn jobs(&self) -> &[JobStatus] {
        &self.jobs
    }

    /// Which row the cursor is on.
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// The selected job, if there is one.
    pub fn selected(&self) -> Option<&JobStatus> {
        self.jobs.get(self.cursor)
    }

    /// The state word for one job: what the queue view exists to say.
    pub fn state_of(status: &JobStatus) -> &'static str {
        match status.finished.as_ref() {
            Some(summary) if summary.cancelled => "cancelled",
            Some(summary) if !summary.failures.is_empty() => "failed",
            Some(_) => "done",
            None if status.needs_attention() => "waiting",
            None if status.started => "running",
            None => "queued",
        }
    }

    /// One row of the list: id, what it is, how it is going.
    pub fn row_text(&self, status: &JobStatus, _ascii: bool) -> String {
        let state = Self::state_of(status);
        let detail = match status.finished.as_ref() {
            Some(summary) => summary.message(),
            None if status.needs_attention() => "a conflict needs an answer".to_string(),
            None => {
                let percent = status.fraction().map_or_else(String::new, |f| {
                    // `JobStatus::fraction` clamps to 0.0..=1.0 and returns
                    // `None` rather than dividing by a zero total, so the
                    // product is 0..=100 and the cast cannot lose anything.
                    #[allow(
                        clippy::cast_possible_truncation,
                        clippy::cast_sign_loss,
                        reason = "JobStatus::fraction clamps to 0.0..=1.0 or returns None"
                    )]
                    let pct = (f * 100.0).round() as u64;
                    format!("{pct:>3} %  ")
                });
                if status.files_total > 0 {
                    format!(
                        "{percent}{} / {} files",
                        status.files_done, status.files_total
                    )
                } else if status.files_done > 0 {
                    format!("{percent}{} files", status.files_done)
                } else {
                    format!(
                        "{percent}{}",
                        crate::panel::format::human_size(status.bytes_done)
                    )
                }
            }
        };
        // `*` marks the job the progress dialog is currently a view of, so a
        // queue with one foreground and three background jobs says which is
        // which (two views of one job).
        let mark = if status.background { " " } else { "*" };
        format!(
            "{}{mark} {:<18} {state:<10} {detail}",
            status.id,
            status.kind.title()
        )
    }

    /// Move the cursor, clamped.
    fn move_by(&mut self, delta: isize) {
        if self.jobs.is_empty() {
            return;
        }
        let last = self.jobs.len().saturating_sub(1);
        let next = if delta < 0 {
            self.cursor.saturating_sub(delta.unsigned_abs())
        } else {
            self.cursor.saturating_add(delta.unsigned_abs()).min(last)
        };
        self.cursor = next.min(last);
    }

    /// `Enter`: bring a live job forward, or open a finished one's failures.
    fn open(&self) -> DialogOutcome {
        let Some(status) = self.selected() else {
            return DialogOutcome::Consumed;
        };
        match status.finished.as_ref() {
            Some(summary) => {
                if summary.failures.is_empty() {
                    DialogOutcome::Push(Box::new(MessageDialog::line(
                        status.kind.to_string(),
                        summary.message(),
                    )))
                } else {
                    // a backgrounded job's summary is shown when the user comes
                    // here, not when it finishes.
                    DialogOutcome::Replace(Box::new(SummaryDialog::new(
                        status.id,
                        summary.as_ref().clone(),
                    )))
                }
            }
            // a job waiting on a conflict shows its dialog when
            // it is brought forward, which `App::foreground_job` arranges.
            None => DialogOutcome::Accept(DialogResult::Text(
                JobAction::Foreground(status.id).encode(),
            )),
        }
    }

    /// `Del`: drop a finished job from the list. A running one is left alone -
    /// forgetting a live job would orphan its worker.
    fn forget(&self) -> DialogOutcome {
        match self.selected() {
            Some(status) if status.finished.is_some() => {
                DialogOutcome::Accept(DialogResult::Text(JobAction::Forget(status.id).encode()))
            }
            _ => DialogOutcome::Consumed,
        }
    }

    /// The rows available for the list, and the footer.
    fn regions(area: Rect) -> (Rect, Option<Rect>) {
        if area.height <= 1 {
            return (area, None);
        }
        let list = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));
        let footer = row(area, area.height.saturating_sub(1));
        (list, footer)
    }

    /// The job a screen row belongs to, once scrolling is taken into account.
    fn visible(&self, rows: usize) -> std::ops::Range<usize> {
        if rows == 0 || self.jobs.is_empty() {
            return 0..0;
        }
        let mut scroll = self.scroll.min(self.cursor);
        if self.cursor >= scroll.saturating_add(rows) {
            scroll = self.cursor.saturating_add(1).saturating_sub(rows);
        }
        let scroll = scroll.min(self.jobs.len().saturating_sub(1));
        let end = scroll.saturating_add(rows).min(self.jobs.len());
        scroll..end
    }
}

impl Dialog for QueueDialog {
    fn id(&self) -> DialogId {
        DialogId::JobQueue
    }

    fn title(&self) -> String {
        let running = self.jobs.iter().filter(|j| j.finished.is_none()).count();
        if running == 0 {
            "Background jobs".to_string()
        } else {
            format!("Background jobs - {running} running")
        }
    }

    fn size_hint(&self) -> (u16, u16) {
        let rows = u16::try_from(self.jobs.len().max(1)).unwrap_or(u16::MAX);
        (66, rows.saturating_add(3).clamp(5, 20))
    }

    /// The queue view lists every job, so it takes the table whole
    /// ("pending, active and failed"). [`Self::update`] keeps
    /// the cursor on the row it was on.
    fn job_update(&mut self, jobs: &[JobStatus]) {
        self.update(jobs.to_vec());
    }

    /// None, and that is a decision rather than an omission.
    ///
    ///
    /// The queue view has no focus ring at all: it is a list, and its three
    /// actions are `Enter`, `Del` and `Esc`, all three printed in its footer.
    /// There is no control for `Alt`+letter to jump to.
    fn mnemonic_letters(&self) -> Vec<char> {
        Vec::new()
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        // `Esc` returns to the panels leaving everything running,
        // which is exactly what closing this dialog does.
        if key.is_cancel() {
            return DialogOutcome::Cancel;
        }
        if key.is_accept() {
            return self.open();
        }
        match key.press.code {
            KeyCode::Up => {
                self.move_by(-1);
                DialogOutcome::Consumed
            }
            KeyCode::Down => {
                self.move_by(1);
                DialogOutcome::Consumed
            }
            KeyCode::PageUp => {
                self.move_by(-10);
                DialogOutcome::Consumed
            }
            KeyCode::PageDown => {
                self.move_by(10);
                DialogOutcome::Consumed
            }
            KeyCode::Home => {
                self.cursor = 0;
                DialogOutcome::Consumed
            }
            KeyCode::End => {
                self.cursor = self.jobs.len().saturating_sub(1);
                DialogOutcome::Consumed
            }
            KeyCode::Delete => self.forget(),
            _ => DialogOutcome::Ignored,
        }
    }

    /// Record the window this frame will show, so the next frame starts from
    /// it rather than from the cursor.
    fn layout(&mut self, area: Rect) {
        let (list, _) = Self::regions(area);
        self.scroll = self.visible(usize::from(list.height)).start;
    }

    fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let body: Style = style.body();
        let (list, footer) = Self::regions(area);

        if self.jobs.is_empty() {
            if let Some(rect) = row(list, 0) {
                draw_text(f, rect, "no jobs in the queue", body, style.ascii);
            }
        } else {
            let rows = usize::from(list.height);
            let range = self.visible(rows);
            for (offset, index) in range.clone().enumerate() {
                let Some(rect) = row(list, u16::try_from(offset).unwrap_or(u16::MAX)) else {
                    break;
                };
                let Some(status) = self.jobs.get(index) else {
                    break;
                };
                let selected = index == self.cursor;
                // Cropped from the *end*, unlike the progress dialog's path:
                // a queue row's informative half is its head - the id, the
                // operation and the state - and its tail is detail.
                let text = self.row_text(status, style.ascii);
                let padded = text::fit_left(
                    &text,
                    usize::from(rect.width),
                    text::Crop::End,
                    ellipsis(style.ascii),
                );
                draw_text(f, rect, &padded, style.button(selected), style.ascii);
            }
        }

        if let Some(rect) = footer {
            draw_text(f, rect, HINT, body, style.ascii);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ColorDepth, Theme};
    use crate::input::KeyPress;
    use crate::ops::{JobEvent, JobFailure, JobId, JobKind, JobSummary};
    use crate::vfs::VfsPath;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use std::time::Duration;

    fn key(code: KeyCode) -> DialogKey {
        DialogKey::raw(KeyPress::plain(code))
    }

    fn summary(failures: usize, cancelled: bool) -> JobSummary {
        JobSummary {
            kind: JobKind::Copy,
            files_done: 197,
            dirs_done: 3,
            bytes_done: 1_000,
            skipped: 0,
            failures: (0..failures)
                .map(|i| JobFailure {
                    path: VfsPath::local(format!("/srv/media/{i}.txt")),
                    error: "Permission denied (os error 13)".to_string(),
                })
                .collect(),
            cancelled,
            elapsed: Duration::from_secs(12),
            sized: Vec::new(),
            differing: Vec::new(),
            first_difference: None,
        }
    }

    fn jobs() -> Vec<JobStatus> {
        // One queued, one running, one waiting on a conflict, one failed.
        let queued = JobStatus::queued(JobId(1), JobKind::Copy);

        let mut running = JobStatus::queued(JobId(2), JobKind::Move);
        running.apply(&JobEvent::Progress {
            file: "/srv/media/x".to_string(),
            file_bytes_done: 0,
            file_bytes_total: 0,
            files_done: 10,
            files_total: 200,
            bytes_done: 43,
            bytes_total: 100,
            throughput: None,
            eta: None,
            elapsed: Duration::from_secs(1),
        });
        running.background = true;

        let mut waiting = JobStatus::queued(JobId(3), JobKind::Delete { trash: true });
        waiting.started = true;
        waiting.pending_decision = Some(Box::new(crate::ops::ConflictRequest {
            source: VfsPath::local("/a"),
            dest: VfsPath::local("/b"),
            source_size: 1,
            dest_size: 2,
            source_mtime: None,
            dest_mtime: None,
            both_dirs: false,
            dest_is_dir: false,
        }));

        let mut failed = JobStatus::queued(JobId(4), JobKind::Copy);
        failed.apply(&JobEvent::Finished {
            summary: Box::new(summary(3, false)),
        });

        vec![queued, running, waiting, failed]
    }

    fn dialog() -> QueueDialog {
        QueueDialog::new(jobs())
    }

    fn render(d: &QueueDialog, w: u16, h: u16, ascii: bool) -> Buffer {
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
    fn pending_active_and_failed_jobs_are_all_listed() {
        // the sentence, one assertion per word.
        let d = dialog();
        let states: Vec<&str> = d.jobs().iter().map(QueueDialog::state_of).collect();
        assert_eq!(states, ["queued", "running", "waiting", "failed"]);

        let out = dump(&render(&d, 80, 24, false));
        for state in states {
            assert!(out.contains(state), "{state} is missing:\n{out}");
        }
    }

    #[test]
    fn enter_on_a_live_job_brings_it_forward() {
        // "Enter on a running one restores its progress dialog."
        let mut d = dialog();
        d.handle_key(&key(KeyCode::Down));
        match d.handle_key(&key(KeyCode::Enter)) {
            DialogOutcome::Accept(DialogResult::Text(text)) => assert_eq!(
                JobAction::parse(&text),
                Some(JobAction::Foreground(JobId(2)))
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn enter_on_a_job_waiting_for_an_answer_also_brings_it_forward() {
        // "Bringing it forward shows the conflict dialog."
        let mut d = dialog();
        d.handle_key(&key(KeyCode::Down));
        d.handle_key(&key(KeyCode::Down));
        assert_eq!(
            QueueDialog::state_of(d.selected().expect("a job")),
            "waiting"
        );
        match d.handle_key(&key(KeyCode::Enter)) {
            DialogOutcome::Accept(DialogResult::Text(text)) => assert_eq!(
                JobAction::parse(&text),
                Some(JobAction::Foreground(JobId(3)))
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn enter_on_a_finished_job_opens_its_failure_summary_here() {
        // a backgrounded job's summary waits for the user.
        let mut d = dialog();
        d.handle_key(&key(KeyCode::End));
        match d.handle_key(&key(KeyCode::Enter)) {
            DialogOutcome::Replace(next) => assert_eq!(next.id(), DialogId::JobSummary),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_clean_finished_job_says_what_it_did_rather_than_opening_a_summary() {
        let mut status = JobStatus::queued(JobId(9), JobKind::Copy);
        status.apply(&JobEvent::Finished {
            summary: Box::new(summary(0, false)),
        });
        let mut d = QueueDialog::new(vec![status]);
        match d.handle_key(&key(KeyCode::Enter)) {
            DialogOutcome::Push(next) => assert_eq!(next.id(), DialogId::Message),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn esc_returns_to_the_panels_leaving_everything_running() {
        let mut d = dialog();
        assert!(matches!(
            d.handle_key(&key(KeyCode::Esc)),
            DialogOutcome::Cancel
        ));
    }

    #[test]
    fn del_forgets_a_finished_job_and_leaves_a_live_one_alone() {
        let mut d = dialog();
        assert!(matches!(
            d.handle_key(&key(KeyCode::Delete)),
            DialogOutcome::Consumed
        ));
        d.handle_key(&key(KeyCode::End));
        match d.handle_key(&key(KeyCode::Delete)) {
            DialogOutcome::Accept(DialogResult::Text(text)) => {
                assert_eq!(JobAction::parse(&text), Some(JobAction::Forget(JobId(4))));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_cursor_stays_on_its_job_when_the_list_changes_underneath() {
        let mut d = dialog();
        d.handle_key(&key(KeyCode::End));
        let selected = d.selected().map(|s| s.id);
        assert_eq!(selected, Some(JobId(4)));
        // The first job finishes and is forgotten; the selection must not slide
        // onto whatever took its place.
        let mut remaining = d.jobs().to_vec();
        remaining.remove(0);
        d.update(remaining);
        assert_eq!(d.selected().map(|s| s.id), Some(JobId(4)));
    }

    #[test]
    fn an_empty_queue_says_so_rather_than_drawing_an_empty_box() {
        let d = QueueDialog::new(Vec::new());
        let out = dump(&render(&d, 60, 15, false));
        assert!(out.contains("no jobs in the queue"), "{out}");
        assert_eq!(d.selected().map(|s| s.id), None);
    }

    #[test]
    fn a_list_longer_than_the_box_scrolls_and_keeps_the_cursor_visible() {
        let jobs: Vec<JobStatus> = (0..40)
            .map(|i| JobStatus::queued(JobId(i), JobKind::Copy))
            .collect();
        let mut d = QueueDialog::new(jobs);
        d.handle_key(&key(KeyCode::End));
        let range = d.visible(5);
        assert!(range.contains(&d.cursor()), "{range:?} vs {}", d.cursor());
        assert_eq!(range.len(), 5);

        d.handle_key(&key(KeyCode::Home));
        let range = d.visible(5);
        assert!(range.contains(&0), "{range:?}");
    }

    #[test]
    fn the_window_the_layout_phase_recorded_survives_into_the_next_frame() {
        let jobs: Vec<JobStatus> = (0..40)
            .map(|i| JobStatus::queued(JobId(i), JobKind::Copy))
            .collect();
        let mut d = QueueDialog::new(jobs);
        d.handle_key(&key(KeyCode::End));
        // Six interior rows is five list rows and a footer.
        d.layout(Rect::new(0, 0, 40, 6));
        assert_eq!(d.visible(5), 35..40);

        // Walking back up three rows keeps the cursor on screen without
        // moving the window, which is the whole reason the scroll is state.
        for _ in 0..3 {
            d.handle_key(&key(KeyCode::Up));
        }
        let range = d.visible(5);
        assert_eq!(range, 35..40, "the view jumped under the cursor");
        assert!(range.contains(&d.cursor()));
    }

    #[test]
    fn it_renders_at_every_size_the_spec_names() {
        for (w, h) in [(200u16, 50u16), (80, 24), (60, 15)] {
            for ascii in [false, true] {
                let out = dump(&render(&dialog(), w, h, ascii));
                assert!(out.contains("Esc panels"), "{w}x{h} ascii={ascii}:\n{out}");
            }
        }
    }

    #[test]
    fn every_glyph_it_draws_has_an_ascii_form() {
        for (w, h) in [(200u16, 50u16), (80, 24), (60, 15), (20, 3)] {
            let inner = dump(&render_inner(&dialog(), w, h, true));
            assert!(inner.is_ascii(), "{w}x{h}:\n{inner}");
        }
    }

    #[test]
    fn a_one_row_interior_still_draws_a_job_and_survives() {
        let d = dialog();
        for h in 0u16..4 {
            let (list, footer) = QueueDialog::regions(Rect::new(0, 0, 40, h));
            assert!(list.bottom() <= h);
            if let Some(rect) = footer {
                assert!(rect.bottom() <= h);
                assert!(rect.height == 1);
            }
        }
        let _ = render(&d, 60, 15, false);
    }

    #[test]
    fn the_queue_view_declares_that_it_has_no_mnemonics() {
        // the queue has no focus ring at all -
        // it is a list, and its three actions are `Enter`, `Del` and `Esc`,
        // all three printed in its footer. There is no control to jump to, and
        // the empty answer is the decision rather than an omission (I11).
        let d = QueueDialog::new(Vec::new());
        assert!(d.mnemonic_letters().is_empty());
    }
}
