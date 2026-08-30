//! The connection actor: the one task in this backend that awaits a socket.
//!
//! Everything above it is synchronous and lives on the blocking pool. A
//! [`super::ops::SmbOps`] method builds a [`Command`], sends it down a tokio
//! unbounded sender (whose `send` is neither `async` nor blocking nor able to
//! panic) and parks on a `std::sync::mpsc::Receiver` (whose `recv` blocks any
//! thread and panics on none). Between those two choices there is no call site
//! anyone can add later that turns into a panic.
//!
//! A share is a *tree connect*, not a directory, so this task keeps one
//! [`Tree`] per share it has been asked for and reuses it. Opening a file
//! hands its handle to a task of its own with a cloned `Connection`, which is
//! what keeps a two-gigabyte copy from stopping a directory listing.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use smb2::{SmbClient, Tree};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::error::{Error, Result};
use crate::vfs::Entry;

use super::files::{read_task, write_task};
use super::io::{Reply, SmbReader, SmbWriter};
use super::ops::{Share, entry_from_row, entry_from_stat, translate};

/// What the actor is asked to do. One variant per [`super::ops::SmbOps`]
/// method that touches the wire.
pub(crate) enum Command {
    /// Every disk share the server will name.
    Shares {
        /// The answer.
        reply: Reply<Vec<Share>>,
    },
    /// One directory inside a share.
    List {
        /// The share.
        share: String,
        /// The directory inside it, `""` for its root.
        dir: String,
        /// The answer.
        reply: Reply<Vec<Entry>>,
    },
    /// One path's metadata.
    Stat {
        /// The share.
        share: String,
        /// The path inside it.
        path: String,
        /// The answer.
        reply: Reply<Entry>,
    },
    /// A random-access reader.
    OpenRead {
        /// The share.
        share: String,
        /// The file inside it.
        path: String,
        /// The answer.
        reply: Reply<SmbReader>,
    },
    /// A writer whose flush is the commit.
    OpenWrite {
        /// The share.
        share: String,
        /// The file inside it.
        path: String,
        /// The answer.
        reply: Reply<SmbWriter>,
    },
    /// Create one directory.
    CreateDir {
        /// The share.
        share: String,
        /// The directory inside it.
        path: String,
        /// The answer.
        reply: Reply<()>,
    },
    /// Remove one file.
    RemoveFile {
        /// The share.
        share: String,
        /// The file inside it.
        path: String,
        /// The answer.
        reply: Reply<()>,
    },
    /// Remove one empty directory.
    RemoveDir {
        /// The share.
        share: String,
        /// The directory inside it.
        path: String,
        /// The answer.
        reply: Reply<()>,
    },
    /// Rename within one share.
    Rename {
        /// The share.
        share: String,
        /// From.
        from: String,
        /// To.
        to: String,
        /// The answer.
        reply: Reply<()>,
    },
    /// Stop. Idempotent, because the actor also stops when its sender drops.
    Close,
}

/// The actor loop.
///
/// It ends on `Close`, on its command sender being dropped, and on nothing
/// else. `live` is cleared on the way out, so a panel that is looking at this
/// connection learns it has gone without asking the socket.
pub(crate) async fn run(
    mut client: SmbClient,
    mut commands: UnboundedReceiver<Command>,
    live: Arc<AtomicBool>,
    authority: String,
) {
    let mut trees: HashMap<String, Tree> = HashMap::new();
    while let Some(command) = commands.recv().await {
        let here = &authority;
        match command {
            Command::Close => break,
            Command::Shares { reply } => {
                let outcome = client
                    .list_shares()
                    .await
                    .map(|found| {
                        found
                            .into_iter()
                            .map(|s| Share {
                                name: s.name,
                                comment: s.comment,
                            })
                            .collect()
                    })
                    .map_err(|err| translate(&err, here, here));
                let _ = reply.send(outcome);
            }
            Command::List { share, dir, reply } => {
                let outcome = async {
                    let tree = tree_for(&mut client, &mut trees, &share, here).await?;
                    let rows = client
                        .list_directory(tree, &dir)
                        .await
                        .map_err(|err| translate(&err, here, &share))?;
                    Ok(rows
                        .iter()
                        .filter_map(entry_from_row)
                        .collect::<Vec<Entry>>())
                }
                .await;
                let _ = reply.send(outcome);
            }
            Command::Stat { share, path, reply } => {
                let outcome = async {
                    let tree = tree_for(&mut client, &mut trees, &share, here).await?;
                    let info = client
                        .stat(tree, &path)
                        .await
                        .map_err(|err| translate(&err, here, &path))?;
                    Ok(entry_from_stat(&path, &info))
                }
                .await;
                let _ = reply.send(outcome);
            }
            Command::OpenRead { share, path, reply } => {
                let outcome = async {
                    let tree = tree_for(&mut client, &mut trees, &share, here).await?;
                    let reader = client
                        .open_file_reader(tree, &path)
                        .await
                        .map_err(|err| translate(&err, here, &path))?;
                    let size = reader.size();
                    let (tx, rx) = unbounded_channel();
                    tokio::spawn(read_task(reader, rx, here.clone(), path.clone()));
                    Ok(SmbReader::new(tx, size, here.clone()))
                }
                .await;
                let _ = reply.send(outcome);
            }
            Command::OpenWrite { share, path, reply } => {
                let outcome = async {
                    let tree = tree_for(&mut client, &mut trees, &share, here).await?;
                    let writer = client
                        .create_file_writer(tree, &path)
                        .await
                        .map_err(|err| translate(&err, here, &path))?;
                    let (tx, rx) = unbounded_channel();
                    tokio::spawn(write_task(writer, rx, here.clone(), path.clone()));
                    Ok(SmbWriter::new(tx, here.clone()))
                }
                .await;
                let _ = reply.send(outcome);
            }
            Command::CreateDir { share, path, reply } => {
                let outcome = async {
                    let tree = tree_for(&mut client, &mut trees, &share, here).await?;
                    client
                        .create_directory(tree, &path)
                        .await
                        .map_err(|err| translate(&err, here, &path))
                }
                .await;
                let _ = reply.send(outcome);
            }
            Command::RemoveFile { share, path, reply } => {
                let outcome = async {
                    let tree = tree_for(&mut client, &mut trees, &share, here).await?;
                    client
                        .delete_file(tree, &path)
                        .await
                        .map_err(|err| translate(&err, here, &path))
                }
                .await;
                let _ = reply.send(outcome);
            }
            Command::RemoveDir { share, path, reply } => {
                let outcome = async {
                    let tree = tree_for(&mut client, &mut trees, &share, here).await?;
                    client
                        .delete_directory(tree, &path)
                        .await
                        .map_err(|err| translate(&err, here, &path))
                }
                .await;
                let _ = reply.send(outcome);
            }
            Command::Rename {
                share,
                from,
                to,
                reply,
            } => {
                let outcome = async {
                    let tree = tree_for(&mut client, &mut trees, &share, here).await?;
                    client
                        .rename(tree, &from, &to)
                        .await
                        .map_err(|err| translate(&err, here, &from))
                }
                .await;
                let _ = reply.send(outcome);
            }
        }
    }
    live.store(false, Ordering::SeqCst);
}

/// The tree for a share, connecting it the first time it is asked for.
///
/// One tree connect per share per session, kept for the life of the
/// connection: a panel that walks in and out of a share must not pay a tree
/// connect each time.
async fn tree_for<'a>(
    client: &mut SmbClient,
    trees: &'a mut HashMap<String, Tree>,
    share: &str,
    authority: &str,
) -> Result<&'a mut Tree> {
    if !trees.contains_key(share) {
        let tree = client
            .connect_share(share)
            .await
            .map_err(|err| translate(&err, authority, share))?;
        trees.insert(share.to_string(), tree);
    }
    trees
        .get_mut(share)
        .ok_or_else(|| Error::ConnectionLost(authority.to_string()))
}
