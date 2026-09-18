//! `JobKind::LocalSend` - send the sources to a LocalSend device.
//!
//! The same shape as a download: the network work is
//! [`crate::localsend::send`], and this runner is the join between it and
//! the [`JobContext`] - progress bar, background, queue and cancel all come
//! from being a job. The device and the PIN ride in
//! [`JobOptions::localsend`]; the files are the sources, walked here on the
//! worker rather than on the render thread, because a folder can be large.
//!
//! One `prepare-upload` for everything, then one upload per file. The
//! receiver may leave a file out of the tokens it hands back, which is its
//! way of declining that one file; it is skipped, not failed. A cancel - the
//! flag, or the receiver refusing mid-way - tells the device the session is
//! over before returning, so its prompt does not sit open.

use std::path::{Path, PathBuf};

use super::{JobContext, JobKind, JobOptions, JobSpec};
use crate::localsend::send::{self, Sender};
use crate::localsend::{DeviceInfo, FileMeta, Peer, SendError};
use crate::vfs::{Vfs, VfsPath};

/// What a [`JobKind::LocalSend`] needs beyond its sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSendRequest {
    /// This side, as the `prepare-upload` introduces it.
    pub me: DeviceInfo,
    /// The device to send to.
    pub peer: Peer,
    /// The PIN to offer, when the user typed one.
    pub pin: Option<String>,
}

impl JobOptions {
    /// Options carrying one [`LocalSendRequest`], the rest default.
    #[must_use]
    pub fn localsend(request: LocalSendRequest) -> Self {
        Self {
            localsend: Some(request),
            ..Self::default()
        }
    }
}

/// Send `spec.sources` to `spec.options.localsend`'s device.
pub fn run(_vfs: &dyn Vfs, spec: &JobSpec, ctx: &mut JobContext) {
    debug_assert_eq!(spec.kind, JobKind::LocalSend);
    let Some(request) = spec.options.localsend.as_ref() else {
        ctx.fail(&VfsPath::local_root(), "no device was chosen");
        return;
    };
    let paths: Vec<PathBuf> = spec
        .sources
        .iter()
        .filter_map(|p| p.local_path().map(Path::to_path_buf))
        .collect();
    let Some(first) = paths.first().cloned() else {
        ctx.fail(&VfsPath::local_root(), "only local files can be sent");
        return;
    };
    let files = match send::collect(&paths) {
        Ok(files) => files,
        Err(err) => {
            ctx.fail(&VfsPath::local(&first), err.to_string());
            return;
        }
    };
    let total: u64 = files.iter().map(|(meta, _)| meta.size).sum();
    ctx.start(files.len() as u64, total);

    let sender = Sender::new(request.me.clone());
    let metas: Vec<FileMeta> = files.iter().map(|(meta, _)| meta.clone()).collect();
    let session = match sender.prepare(&request.peer, &metas, request.pin.as_deref()) {
        Ok(session) => session,
        Err(SendError::PinRequired) => {
            ctx.fail(
                &VfsPath::local(&first),
                format!(
                    "{} wants a PIN - type it in the picker and send again",
                    request.peer.alias
                ),
            );
            return;
        }
        Err(err) => {
            ctx.fail(
                &VfsPath::local(&first),
                format!("{}: {err}", request.peer.alias),
            );
            return;
        }
    };

    for (meta, path) in &files {
        if ctx.cancelled() {
            let _ = Sender::cancel(&request.peer, &session);
            return;
        }
        ctx.set_file(&meta.file_name, meta.size);
        if !session.tokens.contains_key(&meta.id) {
            // The receiver chose not to take this one.
            ctx.add_skipped();
            continue;
        }
        let result = Sender::upload(&request.peer, &session, meta, path, &mut |n| {
            ctx.add_bytes(n)
        });
        match result {
            Ok(()) => ctx.add_file(),
            Err(SendError::Cancelled) => {
                let _ = Sender::cancel(&request.peer, &session);
                return;
            }
            Err(err) => {
                ctx.fail(
                    &VfsPath::local(path),
                    format!("{}: {err}", request.peer.alias),
                );
                let _ = Sender::cancel(&request.peer, &session);
                return;
            }
        }
    }
}

#[cfg(test)]
#[path = "localsend_tests.rs"]
mod tests;
