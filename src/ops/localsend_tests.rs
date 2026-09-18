//! The send job against the fake receiver: progress, skips, cancel, PIN.

use super::*;
use crate::localsend::fake::{Mode, Receiver, scratch};
use crate::localsend::{DeviceInfo, Protocol};
use crate::vfs::LocalFs;

fn spec_for(receiver: &Receiver, paths: &[PathBuf], pin: Option<&str>) -> JobSpec {
    let sources = paths.iter().map(VfsPath::local).collect();
    JobSpec::new(JobKind::LocalSend, sources, None).with_options(JobOptions::localsend(
        LocalSendRequest {
            me: DeviceInfo::ours("hcmd test", 1, Protocol::Http),
            peer: receiver.peer(),
            pin: pin.map(str::to_string),
        },
    ))
}

#[test]
fn the_job_sends_every_file_and_counts_files_and_bytes() {
    let (dir, paths) = scratch("job-ok");
    let receiver = Receiver::start(Mode::Accept);
    let (mut ctx, _rx, _decisions, _cancel) = JobContext::for_test(JobKind::LocalSend);
    crate::ops::run(
        &LocalFs::new(),
        &spec_for(&receiver, &paths, None),
        &mut ctx,
    );
    let summary = ctx.finish();
    assert!(summary.failures.is_empty(), "{:?}", summary.failures);
    assert!(!summary.cancelled);
    assert_eq!(summary.files_done, 3);
    assert_eq!(summary.bytes_done, 300_000 + 9 + 6);
    let seen = receiver.seen.lock().expect("lock");
    assert_eq!(seen.uploaded.len(), 3);
    assert_eq!(
        seen.uploaded.get("album/big.bin").map(|(_, b)| b.len()),
        Some(300_000)
    );
    drop(seen);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_declined_device_is_one_failure_naming_it_and_nothing_is_uploaded() {
    let (dir, paths) = scratch("job-no");
    let receiver = Receiver::start(Mode::Decline);
    let (mut ctx, _rx, _decisions, _cancel) = JobContext::for_test(JobKind::LocalSend);
    crate::ops::run(
        &LocalFs::new(),
        &spec_for(&receiver, &paths, None),
        &mut ctx,
    );
    let summary = ctx.finish();
    assert_eq!(summary.failures.len(), 1, "{:?}", summary.failures);
    let why = format!("{:?}", summary.failures[0]);
    assert!(
        why.contains("Fake Phone") && why.contains("declined"),
        "{why}"
    );
    assert_eq!(summary.files_done, 0);
    assert!(receiver.seen.lock().expect("lock").uploaded.is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_pin_the_user_did_not_give_is_said_and_one_they_did_is_used() {
    let (dir, paths) = scratch("job-pin");
    let receiver = Receiver::start(Mode::Pin);
    let (mut ctx, _rx, _decisions, _cancel) = JobContext::for_test(JobKind::LocalSend);
    crate::ops::run(
        &LocalFs::new(),
        &spec_for(&receiver, &paths, None),
        &mut ctx,
    );
    let summary = ctx.finish();
    let why = format!("{:?}", summary.failures);
    assert!(why.contains("wants a PIN"), "{why}");

    let (mut ctx, _rx, _decisions, _cancel) = JobContext::for_test(JobKind::LocalSend);
    crate::ops::run(
        &LocalFs::new(),
        &spec_for(&receiver, &paths, Some("1234")),
        &mut ctx,
    );
    let summary = ctx.finish();
    assert!(summary.failures.is_empty(), "{:?}", summary.failures);
    assert_eq!(summary.files_done, 3);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_cancel_before_the_first_file_tells_the_device_and_sends_nothing() {
    let (dir, paths) = scratch("job-cancel");
    let receiver = Receiver::start(Mode::Accept);
    let (mut ctx, _rx, _decisions, cancel) = JobContext::for_test(JobKind::LocalSend);
    cancel.cancel();
    crate::ops::run(
        &LocalFs::new(),
        &spec_for(&receiver, &paths, None),
        &mut ctx,
    );
    let summary = ctx.finish();
    assert!(summary.cancelled);
    assert_eq!(summary.files_done, 0);
    let seen = receiver.seen.lock().expect("lock");
    assert!(seen.cancelled, "the device heard the cancel");
    assert!(seen.uploaded.is_empty());
    drop(seen);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_job_with_no_device_or_no_local_source_fails_up_front() {
    let (mut ctx, _rx, _decisions, _cancel) = JobContext::for_test(JobKind::LocalSend);
    let spec = JobSpec::new(JobKind::LocalSend, vec![VfsPath::local("/tmp")], None);
    crate::ops::run(&LocalFs::new(), &spec, &mut ctx);
    let summary = ctx.finish();
    assert!(
        format!("{:?}", summary.failures).contains("no device"),
        "{:?}",
        summary.failures
    );

    let receiver = Receiver::start(Mode::Accept);
    let (mut ctx, _rx, _decisions, _cancel) = JobContext::for_test(JobKind::LocalSend);
    crate::ops::run(&LocalFs::new(), &spec_for(&receiver, &[], None), &mut ctx);
    let summary = ctx.finish();
    assert!(
        format!("{:?}", summary.failures).contains("only local"),
        "{:?}",
        summary.failures
    );
}
