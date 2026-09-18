//! The status folded from events, and the quit prompt read off it.

use super::*;

#[test]
fn a_zero_byte_batch_divides_by_nothing_anywhere() {
    // The obvious division by zero: a batch whose byte total is 0 - one
    // empty file, or a `mkdir`. Every figure the progress dialog asks for
    // has to come back as "no figure" rather than as a NaN, an infinity or
    // a panic.
    assert_eq!(fraction(0, 0), None);
    assert_eq!(fraction(5, 0), None, "done without a total is still no bar");

    let mut status = JobStatus::queued(JobId(1), JobKind::Copy);
    status.apply(&JobEvent::Started {
        kind: JobKind::Copy,
        files_total: 1,
        bytes_total: 0,
    });
    status.files_done = 1;
    assert_eq!(status.fraction(), None, "no batch bar without a total");
    assert_eq!(status.file_fraction(), None);
    assert!(!status.show_file_bar(0), "and no file bar either");

    // A zero-byte transfer is still a transfer, so the quit prompt names it
    // rather than dividing its way into saying nothing.
    assert_eq!(
        running_job_lines(&[status]),
        vec!["Copying: 1 of 1 files".to_string()]
    );
}

#[test]
fn the_quit_prompt_names_every_transfer_and_no_size_walk() {
    // the design names what is still running; a `Size` walk is not a
    // transfer and quitting during one loses nothing.
    let mut copying = JobStatus::queued(JobId(1), JobKind::Copy);
    copying.bytes_done = 2048;
    copying.bytes_total = 8192;
    let mut walking = JobStatus::queued(JobId(2), JobKind::Size);
    walking.files_total = 900;
    let lines = running_job_lines(&[copying, walking]);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].starts_with("Copying:"), "{lines:?}");
    assert!(
        lines[0].contains("2.0 K"),
        "the extent, in bytes: {lines:?}"
    );
}

#[test]
fn a_job_with_no_byte_total_is_still_described() {
    let mut deleting = JobStatus::queued(JobId(1), JobKind::Delete { trash: true });
    deleting.files_total = 12;
    deleting.files_done = 3;
    assert_eq!(
        running_job_lines(&[deleting]),
        vec!["Moving to trash: 3 of 12 files".to_string()]
    );

    // And one that has not reported anything yet says so rather than
    // claiming `0 of 0`.
    let queued = JobStatus::queued(JobId(2), JobKind::Move);
    assert_eq!(
        running_job_lines(&[queued]),
        vec!["Moving: queued".to_string()]
    );
}

#[test]
fn the_list_is_capped_so_the_prompt_fits_a_sixty_column_terminal() {
    // nine named jobs would not fit at 60x15, so the tail
    // becomes a count rather than being dropped silently.
    let jobs: Vec<JobStatus> = (0..9)
        .map(|n| JobStatus::queued(JobId(n), JobKind::Copy))
        .collect();
    let lines = running_job_lines(&jobs);
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert_eq!(lines[3], "and 6 more.");
}

#[test]
fn nothing_running_is_no_prompt_at_all() {
    assert!(running_job_lines(&[]).is_empty());
    let mut done = JobStatus::queued(JobId(1), JobKind::Copy);
    done.apply(&JobEvent::Finished {
        summary: Box::new(JobSummary {
            kind: JobKind::Copy,
            files_done: 1,
            dirs_done: 0,
            bytes_done: 1,
            skipped: 0,
            failures: Vec::new(),
            cancelled: false,
            elapsed: Duration::ZERO,
            sized: Vec::new(),
            differing: Vec::new(),
            first_difference: None,
        }),
    });
    assert!(running_job_lines(&[done]).is_empty());
}

#[test]
fn job_status_folds_the_event_stream() {
    let mut status = JobStatus::queued(JobId(7), JobKind::Copy);
    assert!(!status.has_started());
    assert_eq!(*status.state(), JobState::Queued);
    status.apply(&JobEvent::Started {
        kind: JobKind::Copy,
        files_total: 4,
        bytes_total: 400,
    });
    assert!(status.has_started());
    assert!(status.is_running());
    status.apply(&JobEvent::Progress {
        file: "src/a.txt".to_string(),
        file_bytes_done: 10,
        file_bytes_total: 40,
        files_done: 1,
        files_total: 4,
        bytes_done: 100,
        bytes_total: 400,
        throughput: Some(50),
        eta: Some(Duration::from_secs(6)),
        elapsed: Duration::from_secs(2),
    });
    assert_eq!(status.file, "src/a.txt", "with enough parent to be unique");
    assert_eq!(status.fraction(), Some(0.25), "the batch bar");
    assert_eq!(status.file_fraction(), Some(0.25), "the file bar");
    assert!(status.show_batch_bar(), "four files is a batch");
    assert!(!status.show_file_bar(1024 * 1024), "40 bytes only flashes");
    assert!(status.is_running());
    status.apply(&JobEvent::Finished {
        summary: Box::new(JobSummary {
            kind: JobKind::Copy,
            files_done: 4,
            dirs_done: 0,
            bytes_done: 400,
            skipped: 0,
            failures: Vec::new(),
            cancelled: false,
            elapsed: Duration::from_secs(8),
            sized: Vec::new(),
            differing: Vec::new(),
            first_difference: None,
        }),
    });
    assert!(!status.is_running());
    assert_eq!(status.fraction(), Some(1.0));
}

/// A conflict for the tests to park a job on.
fn conflict() -> Box<ConflictRequest> {
    Box::new(ConflictRequest {
        source: VfsPath::local("/a"),
        dest: VfsPath::local("/b"),
        source_size: 1,
        dest_size: 2,
        source_mtime: None,
        dest_mtime: None,
        both_dirs: false,
        dest_is_dir: false,
        resumable: false,
    })
}

fn finished(cancelled: bool) -> JobEvent {
    JobEvent::Finished {
        summary: Box::new(JobSummary {
            kind: JobKind::Copy,
            files_done: 1,
            dirs_done: 0,
            bytes_done: 1,
            skipped: 0,
            failures: Vec::new(),
            cancelled,
            elapsed: Duration::ZERO,
            sized: Vec::new(),
            differing: Vec::new(),
            first_difference: None,
        }),
    }
}

#[test]
fn a_job_moves_queued_running_blocked_running_finished_and_never_back() {
    let mut status = JobStatus::queued(JobId(1), JobKind::Copy);
    assert!(!status.is_finished() && !status.is_blocked() && !status.is_running());
    status.apply(&JobEvent::NeedsDecision {
        request: conflict(),
    });
    assert!(status.is_blocked(), "a question can arrive before Started");
    assert!(status.has_started());
    assert!(
        status
            .pending_decision()
            .is_some_and(|r| r.dest == VfsPath::local("/b"))
    );
    // Progress while parked does not un-park it: the worker is waiting.
    status.apply(&JobEvent::Started {
        kind: JobKind::Copy,
        files_total: 1,
        bytes_total: 1,
    });
    assert!(status.is_blocked());
    status.unblock();
    assert!(status.is_running());
    assert!(status.pending_decision().is_none());
    status.unblock();
    assert!(
        status.is_running(),
        "unblocking a running job changes nothing"
    );
    status.apply(&finished(false));
    assert!(status.is_finished() && !status.is_presented());
    assert!(status.summary().is_some_and(|s| !s.cancelled));
    // Nothing moves a finished job: a late question is moot, a late start
    // is noise.
    status.apply(&JobEvent::NeedsDecision {
        request: conflict(),
    });
    assert!(status.is_finished() && !status.is_blocked());
    status.apply(&JobEvent::Started {
        kind: JobKind::Copy,
        files_total: 1,
        bytes_total: 1,
    });
    assert!(status.is_finished());
    status.mark_presented();
    assert!(status.is_presented());
}

#[test]
fn the_view_is_orthogonal_and_bringing_forward_clears_a_dismissal() {
    let mut status = JobStatus::queued(JobId(1), JobKind::Copy);
    assert_eq!(status.view(), View::Foreground { dismissed: false });
    assert!(!status.is_background() && !status.is_dismissed());
    status.dismiss();
    assert!(status.is_dismissed(), "Esc keeps the dialog away");
    status.send_to_background();
    assert!(status.is_background() && !status.is_dismissed());
    status.dismiss();
    assert!(
        status.is_background(),
        "nothing to dismiss in the background"
    );
    status.bring_to_foreground();
    assert_eq!(status.view(), View::Foreground { dismissed: false });
    // And none of that touched where the job is in its life.
    assert_eq!(*status.state(), JobState::Queued);
    status.apply(&finished(true));
    status.send_to_background();
    assert!(status.is_finished() && status.is_background());
    status.mark_presented();
    status.mark_presented();
    assert!(status.is_presented(), "presenting twice is presenting");
}
