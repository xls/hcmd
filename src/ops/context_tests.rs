//! The context: cancellation, the failure ledger, and rates that are never
//! fabricated.

use super::*;

#[test]
fn a_zero_byte_batch_has_no_rate_and_no_eta() {
    // The other half of the division-by-zero trap: a rate or an ETA over a
    // batch of no bytes is "no figure", never NaN, infinity or a panic.
    assert_eq!(rate_of(0, Duration::from_secs(1)), None);
    assert_eq!(rate_of(100, Duration::ZERO), None);
    let meter = RateMeter::new(Duration::from_secs(3), 4);
    assert_eq!(meter.eta(0, 0), None);
}

#[test]
fn a_rate_is_never_fabricated() {
    // rule 3: below `rate_min_samples` the rate shows `-`
    // and the ETA is omitted rather than guessed.
    let mut meter = RateMeter::new(Duration::from_secs(3), 4);
    assert_eq!(meter.rate(), None, "nothing measured yet");
    assert_eq!(meter.eta(0, 1_000_000), None);
    for _ in 0..3 {
        meter.record(1024);
    }
    assert_eq!(meter.rate(), None, "still under the sample floor");
    meter.record(1024);
    assert_eq!(meter.total(), 4096);
    // Four samples in microseconds is a very large rate, but it is a real
    // measurement rather than a guess, so it is reported.
    assert!(meter.rate().is_some());
    assert_eq!(meter.eta(1_000, 1_000), None, "already done");
    assert_eq!(meter.eta(0, 0), None, "no total to divide by");
}

/// rule 2: "a stalled transfer showing a healthy rate is a lie": "The
/// displayed rate is a short moving average (it must be able to show a
/// stall)."
///
/// The window's upper end is **now**, not the last sample - otherwise the
/// numerator, the denominator and the window are all frozen the moment the
/// bytes stop, and the last healthy figure stays on screen. It needs no
/// hang to reach: a batch of one large file followed by ten thousand empty
/// ones emits progress continuously while never recording a byte.
#[test]
fn a_stalled_transfer_shows_a_stall_rather_than_its_last_healthy_rate() {
    let window = Duration::from_millis(60);
    let mut meter = RateMeter::new(window, 4);
    for _ in 0..8 {
        meter.record(1024 * 1024);
    }
    let moving = meter.rate().expect("a rate while bytes are moving");
    assert!(moving > 0);

    // Nothing more is recorded, and more than a window goes by.
    std::thread::sleep(window * 4);
    let stalled = meter.rate().expect("still a measurement, and it is zero");
    assert_eq!(
        stalled, 0,
        "an idle window is a rate of zero, not the {moving} B/s it was"
    );
    // The ETA is the cumulative average and deliberately does not lurch.
    assert!(meter.average().is_some());
}

#[test]
fn rate_never_divides_by_zero() {
    assert_eq!(rate_of(100, Duration::ZERO), None);
    assert_eq!(rate_of(0, Duration::from_secs(1)), None);
    assert_eq!(rate_of(2048, Duration::from_secs(2)), Some(1024));
}

#[test]
fn a_dropped_receiver_cancels_the_job() {
    let (mut ctx, rx, _dtx, _flag) = JobContext::for_test(JobKind::Copy);
    assert!(!ctx.cancelled());
    drop(rx);
    // The next send notices.
    ctx.set_file("/tmp/a", 0);
    assert!(ctx.cancelled(), "a dropped receiver stops the worker");
}

#[test]
fn the_cancel_flag_stops_the_chunk_loop() {
    let (mut ctx, _rx, _dtx, flag) = JobContext::for_test(JobKind::Copy);
    assert!(ctx.add_bytes(1024));
    flag.cancel();
    assert!(!ctx.add_bytes(1024), "add_bytes reports the stop");
    assert!(ctx.cancelled());
}

#[test]
fn failures_are_collected_and_do_not_stop_the_batch() {
    let (mut ctx, _rx, _dtx, _flag) = JobContext::for_test(JobKind::Delete { trash: false });
    ctx.fail(&VfsPath::local("/a"), "no");
    ctx.fail(&VfsPath::local("/b"), "nope");
    assert!(!ctx.cancelled());
    let summary = ctx.finish();
    assert_eq!(summary.failures.len(), 2);
    assert!(!summary.is_clean());
    assert!(summary.message().contains("2 failed"));
}

/// The trap this seam exists to disarm.
///
/// A caller that has already flattened its [`Error`] to a sentence used to
/// get `fatal: false` by construction, whatever the sentence said, so a
/// dead connection reached the summary as an ordinary failure and the
/// batch carried on against it. Recognising the wording is the net; the
/// variant is still the right answer where one is in hand.
#[test]
fn a_lost_connection_is_fatal_even_after_it_has_been_flattened_to_text() {
    let (mut ctx, _rx, _dtx, _flag) = JobContext::for_test(JobKind::Copy);
    ctx.fail(
        &VfsPath::local("/a"),
        Error::connection_lost("sftp://user@host:22").to_string(),
    );
    assert!(ctx.fatal(), "the sentence still names a dead connection");

    let (mut ctx, _rx, _dtx, _flag) = JobContext::for_test(JobKind::Copy);
    ctx.fail(
        &VfsPath::local("/a"),
        Error::connection_closed("sftp://user@host:22").to_string(),
    );
    assert!(ctx.fatal(), "and so does a closed one");
}

/// The regression itself: a backend that spelled the sentence out instead
/// of returning the variant.
///
/// `vfs::archive::session` did exactly this - an `Error::msg` whose text
/// was character for character [`Error::ConnectionClosed`]'s `Display` -
/// so a batch extracting out of an archive on a connection that had gone
/// away produced one row per remaining member. The site now returns the
/// variant; this is the net that catches the next one.
#[test]
fn a_hand_rolled_connection_sentence_is_still_fatal() {
    let hand_rolled = Error::msg(format!(
        "/srv/x.tar#/a: {}",
        crate::error::CONNECTION_CLOSED_TEXT
    ));
    assert!(is_fatal(&hand_rolled), "{hand_rolled}");
    let ordinary = Error::msg("/srv/x.tar#/a: that member is not in the index");
    assert!(!is_fatal(&ordinary), "{ordinary}");
}

/// The other half: a refusal this program decided by itself is not fatal,
/// and says so rather than defaulting to it.
#[test]
fn a_refusal_the_program_decided_itself_does_not_stop_the_batch() {
    let (mut ctx, _rx, _dtx, _flag) = JobContext::for_test(JobKind::Copy);
    ctx.fail(
        &VfsPath::local("/a"),
        FailReason::refused("a directory cannot be copied into itself"),
    );
    assert!(!ctx.fatal());
    assert_eq!(ctx.finish().failures.len(), 1);
}

/// A runner's loop, in miniature: twenty files, the connection gone at the
/// second, one failure row rather than twenty identical ones.
#[test]
fn a_batch_stops_at_the_first_lost_connection() {
    let (mut ctx, _rx, _dtx, _flag) = JobContext::for_test(JobKind::Copy);
    let mut attempted = 0;
    for n in 0..20 {
        attempted += 1;
        let path = VfsPath::local(format!("/src/file{n}"));
        if n == 0 {
            ctx.fail(&path, FailReason::refused("something ordinary"));
        } else {
            ctx.fail(&path, Error::connection_lost("sftp://user@host:22"));
        }
        if ctx.fatal() {
            break;
        }
    }
    assert_eq!(attempted, 2, "the loop stopped at the drop");
    assert_eq!(ctx.finish().failures.len(), 2);
}

/// The subject of a failure can be compared against the path that was
/// asked for, which is what tells "this file is missing" from "something
/// else went missing while we were working".
#[test]
fn a_failure_names_a_subject_that_can_be_compared() {
    let missing = Error::not_found("/srv/gone");
    assert!(missing.is_about(&"/srv/gone"));
    assert!(!missing.is_about(&"/srv/here"));

    let bad = Error::invalid_path("/srv/x", "a remote path has to be valid UTF-8");
    assert_eq!(bad.subject(), crate::error::Subject::Path("/srv/x"));

    let gone = Error::connection_lost("sftp://user@host:22");
    assert_eq!(
        gone.subject(),
        crate::error::Subject::Connection("sftp://user@host:22"),
        "the port is not mistaken for the start of an explanation"
    );
    assert_eq!(
        Error::msg("no idea").subject(),
        crate::error::Subject::Nothing
    );
}
