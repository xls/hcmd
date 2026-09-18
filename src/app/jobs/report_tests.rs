//! The finish report as a pure function of kind, outcome and view.

use super::*;
use crate::ops::JobFailure;

/// A summary of `kind` with `failures`, cancelled or not.
fn summary(kind: JobKind, failures: Vec<JobFailure>, cancelled: bool) -> JobSummary {
    JobSummary {
        kind,
        files_done: 1,
        dirs_done: 0,
        bytes_done: 10,
        skipped: 0,
        failures,
        cancelled,
        elapsed: std::time::Duration::ZERO,
        sized: Vec::new(),
        differing: Vec::new(),
        first_difference: None,
    }
}

fn failure(path: &str, error: &str) -> JobFailure {
    JobFailure {
        path: VfsPath::local(path),
        error: error.to_string(),
    }
}

const FOREGROUND: View = View::Foreground { dismissed: false };

#[test]
fn a_background_finish_reports_nothing_whatever_happened() {
    let clean = summary(JobKind::Copy, Vec::new(), false);
    let failed = summary(JobKind::Copy, vec![failure("/a", "no")], false);
    assert_eq!(report_for(View::Background, &clean), Report::Nothing);
    assert_eq!(report_for(View::Background, &failed), Report::Nothing);
}

#[test]
fn a_clean_or_cancelled_foreground_finish_is_one_status_line() {
    let clean = summary(JobKind::Move, Vec::new(), false);
    assert_eq!(
        report_for(FOREGROUND, &clean),
        Report::Message(clean.message())
    );
    // Cancelled with a stray failure for the file it was interrupted on:
    // still a line, never the retry box.
    let cancelled = summary(JobKind::Copy, vec![failure("/tmp/x", "cancelled")], true);
    assert_eq!(
        report_for(FOREGROUND, &cancelled),
        Report::Message(cancelled.message())
    );
    // Dismissed is still foreground: the user closed the dialog, not the
    // job, and its result is still theirs to read.
    assert_eq!(
        report_for(View::Foreground { dismissed: true }, &clean),
        Report::Message(clean.message())
    );
}

#[test]
fn a_failed_foreground_finish_is_the_summary_box() {
    let failed = summary(JobKind::Copy, vec![failure("/a", "no")], false);
    assert_eq!(
        report_for(FOREGROUND, &failed),
        Report::Failures {
            show_summary: true,
            offer_permanent_delete: Vec::new(),
        }
    );
}

#[test]
fn a_trash_delete_with_nowhere_to_trash_to_offers_the_question_instead_of_the_box() {
    // Every failure is "no trash here": the offer says it all, no box.
    let mut all_untrashable = summary(
        JobKind::Delete { trash: true },
        vec![failure(
            "/mnt/usb/a",
            &format!("could not trash{}", crate::ops::delete::NO_TRASH_HERE),
        )],
        false,
    );
    all_untrashable.files_done = 0;
    let report = report_for(FOREGROUND, &all_untrashable);
    match report {
        Report::Failures {
            show_summary,
            offer_permanent_delete,
        } => {
            assert!(!show_summary, "nothing the offer does not already say");
            assert_eq!(offer_permanent_delete, vec![VfsPath::local("/mnt/usb/a")]);
        }
        other => panic!("{other:?}"),
    }
    // Mixed failures keep the box and still make the offer.
    let mixed = summary(
        JobKind::Delete { trash: true },
        vec![
            failure(
                "/mnt/usb/a",
                &format!("could not trash{}", crate::ops::delete::NO_TRASH_HERE),
            ),
            failure("/home/t/b", "Permission denied"),
        ],
        false,
    );
    match report_for(FOREGROUND, &mixed) {
        Report::Failures {
            show_summary,
            offer_permanent_delete,
        } => {
            assert!(show_summary);
            assert_eq!(offer_permanent_delete.len(), 1);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_kinds_without_a_dialog_report_in_the_status_line_even_from_the_background() {
    let walk = summary(JobKind::Size, Vec::new(), false);
    assert_eq!(report_for(View::Background, &walk), Report::Nothing);
    let partial = summary(
        JobKind::Size,
        vec![failure("/a", "Permission denied"), failure("/b", "gone")],
        false,
    );
    assert_eq!(
        report_for(View::Background, &partial),
        Report::Message("/a: Permission denied (and 1 more)".to_string())
    );
    let made = summary(JobKind::Mkdir, Vec::new(), false);
    assert_eq!(
        report_for(View::Background, &made),
        Report::Message(made.message())
    );
    let renamed = summary(JobKind::Rename, vec![failure("/a", "exists")], false);
    match report_for(View::Background, &renamed) {
        Report::Message(line) => assert!(line.ends_with("; see the result list"), "{line}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_comparisons_have_reports_of_their_own() {
    assert_eq!(
        report_for(FOREGROUND, &summary(JobKind::Compare, Vec::new(), false)),
        Report::Compare
    );
    assert_eq!(
        report_for(
            View::Background,
            &summary(JobKind::CompareFiles, Vec::new(), false)
        ),
        Report::CompareFiles
    );
}
