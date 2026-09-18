use super::*;

#[test]
fn a_fresh_get_writes_the_whole_file_from_the_start() {
    // No resume offset, an ordinary 200: rewrite from zero with the stated
    // size.
    assert_eq!(
        plan(0, 200, Some(1000), None).expect("200 is fine"),
        Plan::Write {
            offset: 0,
            total: Some(1000)
        }
    );
}

#[test]
fn an_honoured_range_continues_from_the_offset() {
    // 206 with a partial length: the finished size is the offset plus what is
    // still to come.
    assert_eq!(
        plan(200, 206, Some(800), None).expect("206 is fine"),
        Plan::Write {
            offset: 200,
            total: Some(1000)
        }
    );
    // And a Content-Range total is trusted over the arithmetic when present.
    assert_eq!(
        plan(200, 206, Some(800), Some(1000)).expect("206 is fine"),
        Plan::Write {
            offset: 200,
            total: Some(1000)
        }
    );
}

#[test]
fn a_server_that_ignores_the_range_restarts_the_file() {
    // We asked to resume at 200, but the server answered 200 with the whole
    // file: the half-download is discarded rather than appended to.
    assert_eq!(
        plan(200, 200, Some(1000), None).expect("200 is fine"),
        Plan::Write {
            offset: 0,
            total: Some(1000)
        }
    );
}

#[test]
fn a_satisfied_range_is_nothing_to_fetch() {
    // 416: the partial file is already the whole resource.
    assert_eq!(
        plan(1000, 416, None, None).expect("416 is complete"),
        Plan::Complete
    );
}

#[test]
fn a_failure_status_is_an_error_not_a_write() {
    assert!(plan(0, 404, None, None).is_err());
    assert!(plan(0, 403, None, None).is_err());
    assert!(plan(200, 500, None, None).is_err());
}

#[test]
fn the_content_range_total_is_read_after_the_slash() {
    assert_eq!(range_total("bytes 0-99/1000"), Some(1000));
    assert_eq!(range_total("bytes 200-999/1000"), Some(1000));
    // An unknown total is not a size.
    assert_eq!(range_total("bytes 0-99/*"), None);
    // Garbage is not a size either.
    assert_eq!(range_total("nonsense"), None);
    assert_eq!(range_total(""), None);
}
