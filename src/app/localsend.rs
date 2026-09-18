//! `Alt+X`: send the selection to a LocalSend device.
//!
//! The keystroke collects what is selected; the event loop starts discovery
//! and puts the picker up; while the picker is open the loop feeds it every
//! device heard and re-announces now and then; choosing a device queues a
//! [`JobKind::LocalSend`], which sends as a job - progress bar, background,
//! cancel - and the picker and the discovery go away. Discovery lives
//! exactly as long as the picker: no socket stays in the multicast group
//! behind a closed dialog.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::app::App;
use crate::localsend::{DeviceInfo, Discovery, Protocol};
use crate::ops::localsend::LocalSendRequest;
use crate::ops::{JobKind, JobOptions, JobSpec};
use crate::ui::dialog::localsend::{Selection, SendDeviceDialog, Summary, decode_choice};
use crate::vfs::VfsPath;

/// How often the picker re-announces, so a device whose app opened after
/// the first announcement is still found.
const REANNOUNCE: Duration = Duration::from_secs(3);

/// The send's state on the application.
#[derive(Default)]
pub struct LocalSending {
    /// Paths a keystroke asked to send; the event loop starts the picker.
    pending: Option<Vec<PathBuf>>,
    /// What the picker is choosing a device for.
    paths: Vec<PathBuf>,
    /// The discovery, while the picker is up.
    discovery: Option<Discovery>,
    /// When the last announcement went out.
    announced: Option<Instant>,
    /// The count of what is going, coming from a walk on its own thread: a
    /// folder can be large, and the render thread does not walk folders.
    counting: Option<std::sync::mpsc::Receiver<Summary>>,
}

/// How many folders and files are in `paths`, at the top level.
fn selection_of(paths: &[PathBuf]) -> Selection {
    let folders = paths.iter().filter(|p| p.is_dir()).count();
    Selection {
        folders,
        files: paths.len().saturating_sub(folders),
    }
}

/// Every file and byte behind `paths`, the way the send will walk them.
fn summarise(paths: &[PathBuf]) -> Summary {
    let files = crate::localsend::collect(paths).unwrap_or_default();
    Summary {
        files: files.len() as u64,
        bytes: files.iter().map(|(meta, _)| meta.size).sum(),
    }
}

impl std::fmt::Debug for LocalSending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalSending")
            .field("pending", &self.pending)
            .field("paths", &self.paths)
            .field("discovering", &self.discovery.is_some())
            .finish()
    }
}

/// The name this copy of hcmd gives itself on the other side's screen.
#[must_use]
pub fn alias() -> String {
    let host = crate::console::osc::hostname();
    if host.is_empty() {
        "hcmd".to_string()
    } else {
        format!("hcmd on {host}")
    }
}

impl App {
    /// `Alt+X`: queue a send of the active panel's selection.
    pub fn request_send_device(&mut self) {
        let paths: Vec<PathBuf> = self
            .active_panel()
            .active_tab()
            .operand_paths()
            .iter()
            .filter_map(|p| p.local_path().map(Path::to_path_buf))
            .collect();
        self.send_paths(paths);
    }

    /// Queue a send of these local paths. Split out so a test can send
    /// without a panel listing behind it.
    pub fn send_paths(&mut self, paths: Vec<PathBuf>) {
        if self.localsend.discovery.is_some() || !self.localsend.paths.is_empty() {
            self.message = Some("already choosing a device - finish that first".to_string());
            return;
        }
        if paths.is_empty() {
            self.message =
                Some("nothing to send - select local files or folders in the panel".to_string());
            return;
        }
        self.localsend.pending = Some(paths);
    }

    /// Start discovery for a queued send and put the picker up; while the
    /// picker is up, feed it what discovery hears. Called by the event loop
    /// every turn.
    pub fn service_localsend(&mut self) {
        if let Some(paths) = self.localsend.pending.take() {
            let lan = crate::serve::lan_ip().and_then(|ip| match ip {
                IpAddr::V4(v4) => Some(v4),
                IpAddr::V6(_) => None,
            });
            let selection = selection_of(&paths);
            let (tx, rx) = std::sync::mpsc::channel();
            let to_count = paths.clone();
            std::thread::Builder::new()
                .name("localsend-count".into())
                .spawn(move || {
                    let _ = tx.send(summarise(&to_count));
                })
                .ok();
            self.localsend.counting = Some(rx);
            self.localsend.paths = paths;
            let mut picker = SendDeviceDialog::new(selection);
            // The picker goes up either way: a machine that cannot join the
            // multicast group - a container, a locked-down network - can still
            // send to a typed address, and the picker says what it cannot do.
            match Discovery::start(&alias(), lan) {
                Ok(discovery) => {
                    self.localsend.discovery = Some(discovery);
                    self.localsend.announced = Some(Instant::now());
                }
                Err(err) => picker.not_listening(format!("cannot listen for devices ({err})")),
            }
            self.push_dialog(Box::new(picker));
        }
        let counted = self
            .localsend
            .counting
            .as_ref()
            .and_then(|rx| rx.try_recv().ok());
        if counted.is_some() {
            self.localsend.counting = None;
        }
        let peers = self.localsend.discovery.as_ref().map(|discovery| {
            if self
                .localsend
                .announced
                .is_none_or(|at| at.elapsed() >= REANNOUNCE)
            {
                let _ = discovery.announce();
                self.localsend.announced = Some(Instant::now());
            }
            discovery.peers()
        });
        if counted.is_none() && peers.is_none() {
            return;
        }
        let Some(dialog) = self.top_dialog_mut() else {
            return;
        };
        if let Some(picker) = dialog
            .as_any_mut()
            .and_then(|any| any.downcast_mut::<SendDeviceDialog>())
        {
            if let Some(peers) = peers {
                picker.set_peers(peers);
            }
            if let Some(summary) = counted {
                picker.set_summary(summary);
            }
        }
    }

    /// The picker's answer: send the queued paths to the chosen device.
    pub fn answer_send_device(&mut self, choice: &str) {
        let Some((peer, pin)) = decode_choice(choice) else {
            self.message = Some("no device chosen".to_string());
            self.stop_device_discovery();
            return;
        };
        let paths = std::mem::take(&mut self.localsend.paths);
        let me = self.localsend.discovery.as_ref().map_or_else(
            || DeviceInfo::ours(&alias(), 0, Protocol::Http),
            |d| d.me().clone(),
        );
        self.stop_device_discovery();
        if paths.is_empty() {
            self.message = Some("nothing to send".to_string());
            return;
        }
        let count = paths.len();
        let sources: Vec<VfsPath> = paths.iter().map(VfsPath::local).collect();
        let alias = peer.alias.clone();
        let spec = JobSpec::new(JobKind::LocalSend, sources, None).with_options(
            JobOptions::localsend(LocalSendRequest {
                me,
                peer,
                pin: (!pin.is_empty()).then_some(pin),
            }),
        );
        self.request_job(spec);
        let plural = if count == 1 { "" } else { "s" };
        self.message = Some(format!("sending {count} item{plural} to {alias}"));
    }

    /// Stop looking for devices: the picker's way out, whichever way it went.
    pub fn stop_device_discovery(&mut self) {
        self.localsend.pending = None;
        self.localsend.paths.clear();
        self.localsend.discovery = None;
        self.localsend.announced = None;
        self.localsend.counting = None;
    }

    /// Whether the picker's discovery is running.
    #[must_use]
    pub fn is_discovering_devices(&self) -> bool {
        self.localsend.discovery.is_some()
    }

    /// Whether a picker is choosing a device for some paths.
    #[must_use]
    pub fn is_choosing_device(&self) -> bool {
        !self.localsend.paths.is_empty()
    }

    /// The device a test would send to, built the way the picker builds it.
    #[cfg(test)]
    pub(crate) fn send_paths_queued(&self) -> bool {
        self.localsend.pending.is_some()
    }

    /// A peer the tests can hand straight to [`App::answer_send_device`].
    #[cfg(test)]
    pub(crate) fn encode_peer(peer: &crate::localsend::Peer, pin: &str) -> String {
        crate::ui::dialog::localsend::encode_choice(peer, pin)
    }
}

#[cfg(test)]
#[path = "localsend_tests.rs"]
mod tests;
