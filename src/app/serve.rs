//! `Ctrl+N`: share the selection, for as long as the dialog is open.
//!
//! The keystroke collects what is selected; the event loop starts the
//! listener on the blocking side and pushes the dialog; each request the
//! listener answers comes back as a [`ServeEvent`] and lands in the dialog's
//! log; closing the dialog drops the server, which stops the listener. The
//! same shape as the drive popup and the update check: `dispatch` queues,
//! the loop services, and nothing that touches the network runs on the thread
//! that draws.

use std::path::{Path, PathBuf};

use tokio::sync::mpsc;

use crate::app::App;
use crate::serve::tree::{self, Root};
use crate::serve::{ServeEvent, Server, firewall, lan_ip};
use crate::ui::dialog::serve::ServeDialog;

/// The share's state on the application: what was asked for, and what is
/// running.
#[derive(Debug, Default)]
pub struct Serving {
    /// Roots a keystroke asked to serve; the event loop starts them.
    pending: Option<Vec<Root>>,
    /// The listener, while the dialog is up. Dropping it stops serving.
    server: Option<Server>,
}

impl App {
    /// `Ctrl+N`: queue a share of the active panel's selection.
    pub fn request_serve(&mut self) {
        let paths: Vec<PathBuf> = self
            .active_panel()
            .active_tab()
            .operand_paths()
            .iter()
            .filter_map(|p| p.local_path().map(Path::to_path_buf))
            .collect();
        self.serve_paths(paths);
    }

    /// Queue a share of these local paths.
    ///
    /// Split from [`App::request_serve`] so a test can share a real folder
    /// without a panel listing behind it.
    pub fn serve_paths(&mut self, paths: Vec<PathBuf>) {
        if self.serving.server.is_some() {
            self.message = Some("already serving - close the Serve dialog first".to_string());
            return;
        }
        let roots = tree::roots(&paths);
        if roots.is_empty() {
            self.message =
                Some("nothing to serve - select local files or folders in the panel".to_string());
            return;
        }
        self.serving.pending = Some(roots);
    }

    /// Start the share a keystroke queued and put its dialog up. Called by the
    /// event loop, which owns the channel the listener reports on.
    pub fn service_serve(&mut self, tx: &mpsc::Sender<ServeEvent>) {
        let Some(roots) = self.serving.pending.take() else {
            return;
        };
        let count = roots.len();
        let wanted = self.config.serve.port;
        match Server::start(wanted, roots, tx.clone()) {
            Ok(server) => {
                let port = server.port();
                let mut urls = Vec::new();
                if let Some(ip) = lan_ip() {
                    urls.push(format!("http://{ip}:{port}/"));
                }
                urls.push(format!("http://localhost:{port}/"));
                self.serving.server = Some(server);
                let mut dialog = ServeDialog::new(urls, count);
                if wanted != 0 && port != wanted {
                    dialog.note(format!(
                        "port {wanted} was taken - on {port} instead (serve.port)"
                    ));
                }
                // A few file reads, no process: see the module.
                dialog.firewall(firewall::lines(firewall::detect(port), port));
                self.push_dialog(Box::new(dialog));
            }
            Err(e) => {
                self.message = Some(format!("could not start serving: {e}"));
            }
        }
    }

    /// Fold what the listener did into the dialog, if it is still the one on
    /// top. An event for a dialog already closed is dropped, like a stale
    /// drive-probe answer.
    pub fn apply_serve_event(&mut self, event: ServeEvent) {
        let Some(dialog) = self.top_dialog_mut() else {
            return;
        };
        let Some(serve) = dialog
            .as_any_mut()
            .and_then(|any| any.downcast_mut::<ServeDialog>())
        else {
            return;
        };
        match event {
            ServeEvent::Request(served) => serve.push(&served),
            ServeEvent::Failed(why) => serve.failed(why),
        }
    }

    /// Stop the share: the dialog's way out, and the answer to its `Esc`.
    pub fn stop_serving(&mut self) {
        self.serving.pending = None;
        if self.serving.server.take().is_some() {
            self.message = Some("stopped serving".to_string());
        }
    }

    /// Whether a share is up.
    #[must_use]
    pub fn is_serving(&self) -> bool {
        self.serving.server.is_some()
    }
}

#[cfg(test)]
#[path = "serve_tests.rs"]
mod tests;
