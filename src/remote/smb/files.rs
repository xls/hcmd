//! The two tasks that own an open file, one for each direction.
//!
//! They are the actor's other half: [`super::task::run`] opens the handle and
//! hands it to one of these, which owns it, serves the blocking side over a
//! channel, and closes it on the way out. A cloned `Connection` each is what
//! keeps a two-gigabyte copy from stopping a directory listing.

use super::io::{ReadQueue, WriteMsg, WriteQueue};
use super::ops::translate;

/// The task that owns one open file for reading.
///
/// It ends when the [`SmbReader`] is dropped, which drops the request sender,
/// and it closes the handle on the way out so a share is not left holding
/// opens for files nobody is reading.
pub(crate) async fn read_task(
    reader: smb2::FileReader,
    mut requests: ReadQueue,
    authority: String,
    path: String,
) {
    while let Some(request) = requests.recv().await {
        let outcome = reader
            .read_at(request.at, request.len)
            .await
            .map_err(|err| translate(&err, &authority, &path));
        if request.reply.send(outcome).is_err() {
            break;
        }
    }
    let _ = reader.close().await;
}

/// The task that owns one open file for writing.
///
/// `Finish` is the commit and consumes the writer; every other way out aborts,
/// so a cancelled copy leaves no file pretending to be whole.
pub(crate) async fn write_task(
    mut writer: smb2::FileWriter,
    mut chunks: WriteQueue,
    authority: String,
    path: String,
) {
    while let Some(message) = chunks.recv().await {
        match message {
            WriteMsg::Chunk(data, ack) => {
                let outcome = writer
                    .write_chunk(&data)
                    .await
                    .map_err(|err| translate(&err, &authority, &path));
                let failed = outcome.is_err();
                let _ = ack.send(outcome);
                if failed {
                    return;
                }
            }
            WriteMsg::Finish(reply) => {
                let outcome = writer
                    .finish()
                    .await
                    .map(|_| ())
                    .map_err(|err| translate(&err, &authority, &path));
                let _ = reply.send(outcome);
                return;
            }
            WriteMsg::Abort => {
                let _ = writer.abort().await;
                return;
            }
        }
    }
    // The writer was dropped without a flush, which is a cancelled transfer.
    let _ = writer.abort().await;
}
