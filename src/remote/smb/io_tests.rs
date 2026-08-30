//! The two handles, driven from a thread instead of from a server.

use super::*;
use std::thread;
use tokio::sync::mpsc::unbounded_channel;

/// A reader over `body`, served by a thread that answers positioned reads.
fn reader_over(body: &'static [u8]) -> (SmbReader, thread::JoinHandle<usize>) {
    let (tx, mut rx) = unbounded_channel::<ReadRequest>();
    let server = thread::spawn(move || {
        let mut served = 0usize;
        while let Some(request) = rx.blocking_recv() {
            let at = request.at as usize;
            let end = at.saturating_add(request.len as usize).min(body.len());
            let slice = body.get(at..end).unwrap_or(&[]).to_vec();
            served += 1;
            if request.reply.send(Ok(slice)).is_err() {
                break;
            }
        }
        served
    });
    (
        SmbReader::new(tx, body.len() as u64, "smb://fake".to_string()),
        server,
    )
}

#[test]
fn a_reader_hands_out_every_byte_in_order() {
    let (mut reader, server) = reader_over(b"hello remote world");
    let mut out = Vec::new();
    reader.read_to_end(&mut out).expect("reads");
    assert_eq!(out, b"hello remote world");
    drop(reader);
    let served = server.join().expect("joins");
    assert!(served > 0, "the thread answered at least one request");
}

#[test]
fn a_reader_seeks_without_replaying_what_is_before_the_window() {
    let (mut reader, _server) = reader_over(b"0123456789");
    assert_eq!(reader.seek(SeekFrom::Start(4)).expect("seeks"), 4);
    let mut four = [0u8; 4];
    reader.read_exact(&mut four).expect("reads");
    assert_eq!(&four, b"4567");
    assert_eq!(reader.seek(SeekFrom::End(-2)).expect("seeks"), 8);
    let mut two = [0u8; 2];
    reader.read_exact(&mut two).expect("reads");
    assert_eq!(&two, b"89");
    assert_eq!(reader.stream_position().expect("position"), 10);
    // Past the end reads nothing rather than waiting on a server.
    assert_eq!(reader.read(&mut two).expect("reads"), 0);
}

#[test]
fn a_reader_refuses_a_seek_before_the_start() {
    let (mut reader, _server) = reader_over(b"0123456789");
    let err = reader.seek(SeekFrom::Current(-1)).expect_err("refused");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn a_reader_whose_task_is_gone_reports_the_connection() {
    let (tx, rx) = unbounded_channel::<ReadRequest>();
    drop(rx);
    let mut reader = SmbReader::new(tx, 16, "smb://nas.local:445".to_string());
    let err = reader.read(&mut [0u8; 4]).expect_err("no task");
    assert_eq!(err.kind(), io::ErrorKind::ConnectionAborted);
    let inner = err
        .get_ref()
        .and_then(|e| e.downcast_ref::<Error>())
        .expect("carries the error");
    assert!(matches!(inner, Error::ConnectionLost(_)), "{inner:?}");
}

/// A writer whose thread collects the bytes and reports whether it committed.
fn writer_pair() -> (SmbWriter, thread::JoinHandle<(Vec<u8>, bool)>) {
    let (tx, mut rx) = unbounded_channel::<WriteMsg>();
    let server = thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut committed = false;
        while let Some(message) = rx.blocking_recv() {
            match message {
                WriteMsg::Chunk(data, ack) => {
                    bytes.extend_from_slice(&data);
                    let _ = ack.send(Ok(()));
                }
                WriteMsg::Finish(reply) => {
                    committed = true;
                    let _ = reply.send(Ok(()));
                    break;
                }
                WriteMsg::Abort => break,
            }
        }
        (bytes, committed)
    });
    (SmbWriter::new(tx, "smb://fake".to_string()), server)
}

#[test]
fn a_writer_delivers_every_byte_and_flush_is_the_commit() {
    let (mut writer, server) = writer_pair();
    writer.write_all(b"one ").expect("writes");
    writer.write_all(b"two").expect("writes");
    writer.flush().expect("commits");
    drop(writer);
    let (bytes, committed) = server.join().expect("joins");
    assert_eq!(bytes, b"one two");
    assert!(committed, "flush is the commit");
}

#[test]
fn a_second_flush_is_a_no_op_and_a_write_after_it_is_refused() {
    let (mut writer, server) = writer_pair();
    writer.write_all(b"body").expect("writes");
    writer.flush().expect("commits");
    writer.flush().expect("a second flush does nothing");
    let err = writer.write(b"more").expect_err("refused");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    drop(writer);
    let (bytes, committed) = server.join().expect("joins");
    assert_eq!(bytes, b"body");
    assert!(committed);
}

#[test]
fn a_writer_dropped_without_a_flush_aborts_rather_than_committing() {
    let (mut writer, server) = writer_pair();
    writer.write_all(b"half a file").expect("writes");
    drop(writer);
    let (bytes, committed) = server.join().expect("joins");
    assert_eq!(bytes, b"half a file", "the chunks did arrive");
    assert!(
        !committed,
        "a cancelled transfer must not leave a committed file"
    );
}

#[test]
fn a_writer_whose_task_is_gone_reports_the_connection() {
    let (tx, rx) = unbounded_channel::<WriteMsg>();
    drop(rx);
    let mut writer = SmbWriter::new(tx, "smb://nas.local:445".to_string());
    let err = writer.write(b"body").expect_err("no task");
    assert_eq!(err.kind(), io::ErrorKind::ConnectionAborted);
}

#[test]
fn waiting_on_a_dropped_reply_channel_is_the_connection() {
    let (reply, answer) = std::sync::mpsc::sync_channel::<Result<u8>>(1);
    drop(reply);
    let outcome = wait(answer, "smb://nas.local:445");
    assert!(
        matches!(outcome, Err(Error::ConnectionLost(ref who)) if who == "smb://nas.local:445"),
        "{outcome:?}"
    );
}
