//! The drive popup and the hotlist behind it.
//!
//! # The thread that draws never blocks on I/O
//!
//! Both halves of this popup are I/O: the mount table comes from
//! `/proc/mounts` through `sysinfo`, and deciding whether a hotlist row is
//! still there is a `stat` per entry. Neither may run on the thread that calls
//! `term.terminal().draw()`, and that thread is the event loop - so "queue it
//! and let the event loop service it" is not enough on its own. One hotlist
//! entry on a hung NFS or sshfs mount would otherwise hold the whole
//! application: no key read, no frame drawn, and no way out.
//!
//! So the event loop queues a [`DrivesProbe`], [`probe_drives`] answers it on
//! the blocking pool, and the answer comes back as a [`DrivesEvent`] the
//! `select!` picks up - the shape a directory read already has
//! ([`crate::app::stream_read`] and [`crate::app::VfsEvent`]).
//!
//! # The popup opens with what is already known
//!
//! [`App::service_drives`] pushes the popup on the frame the key was pressed,
//! from what is in memory: the hotlist entries the file was read into, with
//! their `~` and `$VAR` expanded, and no devices yet. The probe's answer
//! replaces it with the complete list, which is the same fill-in-later
//! behaviour the panel has for a directory listing. Nothing waits.
//!
//! `Alt+F1` sets a request and the event loop enumerates. There is one request
//! slot, so `Alt+F1` then `Alt+F2` before a frame has been drawn opens the
//! right panel's popup: the last key the user pressed is the one that wins.
//!
//! It is re-read on a timer only while the popup is open. Polling rather than
//! watching is measured rather than argued: the kernel gives no usable change
//! signal for `/proc/self/mounts` through any pure-Rust crate, `inotify`
//! reports nothing on a mount, and the file's `mtime` never moves. Closing the
//! popup disarms the deadline, so nothing polls when nothing is looking.
//!
//! `Alt+F1` and `Alt+F2` are spatial: each names and acts on the side its key
//! names whichever panel has focus, and focus follows the choice rather than
//! the other way round.

use std::time::Instant;

use tokio::sync::mpsc;

use crate::app::{App, DrivesRequest};
use crate::devices::Device;
use crate::devices::hotlist::{HotlistEntry, HotlistRow};
use crate::input::DialogId;
use crate::ui::dialog::DrivesDialog;

/// The blocking half of a drive popup, queued by the event loop and answered
/// off the thread that draws.
///
/// Carries everything the answer needs, so [`probe_drives`] runs with no
/// `&App` and nothing to borrow: a probe that outlives the popup that asked
/// for it costs a dropped [`DrivesEvent`] and nothing else.
#[derive(Debug, Clone)]
pub enum DrivesProbe {
    /// A popup was asked for. Enumerate the mount table when the popup has a
    /// device half, and `stat` every hotlist entry either way.
    Open {
        /// Which popup, which is also which panel its answer will change.
        request: DrivesRequest,
        /// `devices.show_all`, read here because the probe has no config.
        show_all: bool,
        /// The hotlist as `hotlist.toml` was read into memory.
        entries: Vec<HotlistEntry>,
    },
    /// The open popup's re-enumeration. The hotlist half is untouched by a
    /// refresh, so no entry is stat'ed again.
    Poll {
        /// `devices.show_all`, as above.
        show_all: bool,
    },
}

impl DrivesProbe {
    /// Do the reading. **Blocking, and on the blocking pool only.**
    fn answer(self) -> DrivesEvent {
        match self {
            Self::Open {
                request,
                show_all,
                entries,
            } => {
                // `Ctrl+D`'s hotlist has no device half, so it does not read
                // the mount table at all.
                let devices = match request {
                    DrivesRequest::Devices(_) => crate::devices::enumerate(show_all),
                    DrivesRequest::Hotlist => Vec::new(),
                };
                DrivesEvent::Opened {
                    request,
                    devices,
                    rows: crate::devices::hotlist::rows(&entries),
                }
            }
            Self::Poll { show_all } => DrivesEvent::Polled {
                devices: crate::devices::enumerate(show_all),
            },
        }
    }
}

/// What a finished [`DrivesProbe`] tells the event loop.
#[derive(Debug, Clone)]
pub enum DrivesEvent {
    /// The complete popup the request asked for.
    Opened {
        /// Which popup asked, so an answer for one the user has since left is
        /// dropped rather than drawn.
        request: DrivesRequest,
        /// The mount table, empty for `Ctrl+D`'s hotlist.
        devices: Vec<Device>,
        /// The hotlist, each row now told whether its directory is there.
        rows: Vec<HotlistRow>,
    },
    /// A re-enumeration for the popup that is on screen.
    Polled {
        /// The mount table as it now stands.
        devices: Vec<Device>,
    },
}

/// Answer a [`DrivesProbe`] on the blocking pool and deliver it to the event
/// loop, exactly as [`crate::app::stream_read`] delivers a listing.
///
/// A send that fails means the event loop has gone, and a probe whose thread
/// panicked has no answer to send; both end the task quietly, because there is
/// no one left to tell.
pub async fn probe_drives(probe: DrivesProbe, tx: mpsc::Sender<DrivesEvent>) {
    let Ok(event) = tokio::task::spawn_blocking(move || probe.answer()).await else {
        return;
    };
    let _ = tx.send(event).await;
}

/// One hotlist row as it is known **without touching the disk**.
///
/// Expanding `~` and `$VAR` is text and an environment lookup, so that much of
/// the answer is given now. Whether the directory is still there is the `stat`
/// this module exists to keep off the render thread, so `missing` is left open
/// until [`probe_drives`] answers it: the row draws ungreyed and `Enter` on it
/// navigates, which fails in the panel's own read - off this thread - if the
/// disk is not plugged in after all.
fn unstatted_row(entry: HotlistEntry) -> HotlistRow {
    match crate::devices::hotlist::expand(&entry.path) {
        Ok(resolved) => HotlistRow {
            entry,
            resolved: Some(resolved),
            missing: None,
        },
        Err(why) => HotlistRow {
            entry,
            resolved: None,
            missing: Some(why.to_string()),
        },
    }
}

/// Which dialog a request opens, so an answer can tell whether the popup it
/// was asked for is still the one on screen.
const fn dialog_id(request: DrivesRequest) -> DialogId {
    match request {
        DrivesRequest::Devices(side) => DialogId::Drive(side),
        DrivesRequest::Hotlist => DialogId::Hotlist,
    }
}

impl App {
    /// Queue the popup. `dispatch` calls this; the event loop builds
    /// the list.
    ///
    /// One slot, like every other pending request: `Alt+F1` then `Alt+F2`
    /// before a frame has been drawn means the right panel, which is the last
    /// key the user pressed.
    pub fn request_drives(&mut self, request: DrivesRequest) {
        self.drives.ask(request);
    }

    /// Which popup is queued, if any. For tests and for `drain_input`, which
    /// stops on the frame one is asked for.
    pub const fn drives_pending(&self) -> Option<DrivesRequest> {
        self.drives.asked()
    }

    /// Push the popup and hand back the reading it still needs.
    ///
    /// The popup appears on this frame, built from what is already in memory;
    /// the returned [`DrivesProbe`] is the mount table and the per-entry
    /// `stat`, which the event loop spawns and never performs itself. Neither
    /// half runs here, because this is the thread that draws.
    ///
    /// `Alt+F1` and `Alt+F2` are **spatial**: the popup names and acts on the
    /// side its key names whichever panel has focus, and the focus follows the
    /// choice rather than the other way round (invariant I1).
    pub fn service_drives(&mut self) -> Option<DrivesProbe> {
        let request = self.drives.take()?;
        let mode = self.config.panel.quick_search;
        let case = self.config.panel.quick_search_case;
        let entries = self.hotlist.entries().to_vec();
        let rows: Vec<HotlistRow> = entries.iter().cloned().map(unstatted_row).collect();
        let dialog = match request {
            DrivesRequest::Devices(side) => {
                // the list is live while the popup is open, so the
                // re-enumeration deadline is armed with the popup itself.
                self.drives.arm(Instant::now());
                DrivesDialog::devices(side, Vec::new(), rows).with_anchor(side)
            }
            // the `Ctrl+D` lists no device, so there is nothing to
            // re-enumerate and no deadline to arm.
            DrivesRequest::Hotlist => {
                self.drives.disarm();
                DrivesDialog::hotlist(rows).with_anchor(self.active_side)
            }
        };
        self.push_dialog(Box::new(dialog.with_quick_search(mode, case)));
        Some(DrivesProbe::Open {
            request,
            show_all: self.config.devices.show_all,
            entries,
        })
    }

    /// Ask for a re-enumeration of an open popup once [`crate::devices::POLL`]
    /// has passed (the design; the design).
    ///
    /// `busy` is true while an earlier probe is still unanswered. The deadline
    /// is pushed forward rather than left in the past, because a deadline that
    /// is always due is a loop that never sleeps - and a probe that never
    /// comes back is exactly the hung mount this design is written around, so
    /// it has to be the case that costs nothing.
    ///
    /// Polling rather than watching because the kernel gives no usable change
    /// signal for `/proc/self/mounts` through any pure-Rust crate: `inotify`
    /// reports nothing on a mount and the file's `mtime` never moves, which
    /// the design records as measured rather than argued.
    pub fn service_drives_poll(&mut self, now: Instant, busy: bool) -> Option<DrivesProbe> {
        if !self.drives.is_due(now) {
            return None;
        }
        let Some(dialog) = self.top_dialog_mut() else {
            // The popup was closed; nothing is polling any more.
            self.drives.disarm();
            return None;
        };
        let is_popup = dialog
            .as_any_mut()
            .is_some_and(|any| any.is::<DrivesDialog>());
        if !is_popup {
            self.drives.disarm();
            return None;
        }
        self.drives.arm(now);
        if busy {
            return None;
        }
        Some(DrivesProbe::Poll {
            show_all: self.config.devices.show_all,
        })
    }

    /// Fold a finished [`DrivesProbe`] into the popup on screen.
    ///
    /// An answer for a popup the user has already left is dropped, the way a
    /// [`crate::app::VfsEvent`] with a stale generation is: the dialog on top
    /// is the test, because a popup that has been closed or covered is no
    /// longer the thing this answer describes.
    pub fn apply_drives_event(&mut self, event: DrivesEvent) {
        match event {
            DrivesEvent::Opened {
                request,
                devices,
                rows,
            } => {
                if self.top_dialog().map(crate::dialog::Dialog::id) != Some(dialog_id(request)) {
                    return;
                }
                let mode = self.config.panel.quick_search;
                let case = self.config.panel.quick_search_case;
                // Rebuilt rather than patched row by row: the answer is a
                // whole list and `DrivesDialog` is built from a whole list,
                // which is the one shape both constructors already take. The
                // popup being replaced is the placeholder pushed by
                // `service_drives`, normally a frame old.
                let _ = self.pop_dialog();
                let dialog = match request {
                    DrivesRequest::Devices(side) => {
                        DrivesDialog::devices(side, devices, rows).with_anchor(side)
                    }
                    DrivesRequest::Hotlist => {
                        DrivesDialog::hotlist(rows).with_anchor(self.active_side)
                    }
                };
                self.push_dialog(Box::new(dialog.with_quick_search(mode, case)));
            }
            DrivesEvent::Polled { devices } => {
                let Some(dialog) = self.top_dialog_mut() else {
                    self.drives.disarm();
                    return;
                };
                let Some(drives) = dialog
                    .as_any_mut()
                    .and_then(|any| any.downcast_mut::<DrivesDialog>())
                else {
                    self.drives.disarm();
                    return;
                };
                // In place, so the cursor stays on the mount it was on: a
                // refresh a second apart must not move what the user is aiming
                // at.
                drives.refresh_devices(devices);
            }
        }
    }

    /// When [`App::service_drives_poll`] next has something to do.
    pub const fn drives_deadline(&self) -> Option<Instant> {
        self.drives.deadline()
    }

    /// Read `hotlist.toml` at startup, like [`App::load_hosts`].
    pub fn load_hotlist(&mut self) {
        let (entries, warnings) = crate::devices::hotlist::load();
        self.hotlist.adopt(entries);
        self.warnings.extend(warnings);
    }

    /// The hotlist as it needs writing, or `None` when the file and memory
    /// already agree. The event loop's, like [`App::take_hosts_write`].
    ///
    /// The write itself is a `create_dir_all`, a serialise and a `std::fs::write`
    /// on the config directory, which is I/O and therefore not this thread's:
    /// the caller spawns it and reports what came back on the status line,
    /// because the design keeps configuration problems non-fatal.
    ///
    /// The entries are handed over and the book marked clean in the same act,
    /// so a change made while the write is in flight is the next write rather
    /// than a second copy of this one. A write that failed is reported once
    /// and not retried on every frame for the rest of the session, which is
    /// what the flag meant before the write moved off this thread.
    pub fn take_hotlist_write(&mut self) -> Option<Vec<HotlistEntry>> {
        if !self.hotlist.is_dirty() {
            return None;
        }
        let entries = self.hotlist.entries().to_vec();
        // `adopt` is how this type is told the file and memory agree again,
        // and handing the entries to the writer is the moment that becomes
        // true as far as anything queued here is concerned.
        self.hotlist.adopt(entries.clone());
        Some(entries)
    }

    /// `Ctrl+Shift+D`'s answer: add or relabel the active panel's directory.
    ///
    ///
    /// **A duplicate path replaces the existing entry's label where it
    /// stands** rather than adding a second row, and the order is never
    /// touched: `hotlist.toml` holds the entries in the order the user put
    /// them in (invariant I6).
    pub fn add_to_hotlist(&mut self, label: String, path: String) {
        let replaced = self.hotlist.upsert(label, path.clone());
        self.message = Some(if replaced {
            format!("{path}: relabelled in the hotlist")
        } else {
            format!("{path}: added to the hotlist")
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::app_with;
    use crate::config::{Config, Keymap, Theme};
    use crate::panel::Side;
    use crate::ui::dialog::DriveRow;

    /// A path that cannot exist, so a `stat` of it has a definite answer and
    /// the absence of that answer is proof no `stat` was made.
    const GONE: &str = "/nonexistent-hcmd-hotlist-target/deeper";

    /// The hotlist rows of the popup on screen.
    fn popup_rows(app: &mut App) -> Vec<DriveRow> {
        app.top_dialog_mut()
            .and_then(|dialog| dialog.as_any_mut())
            .and_then(|any| any.downcast_mut::<DrivesDialog>())
            .map(|drives| drives.rows().to_vec())
            .unwrap_or_default()
    }

    /// The `missing` reason of the one hotlist row, and whether there was one.
    fn missing(rows: &[DriveRow]) -> Option<Option<String>> {
        rows.iter().find_map(|row| match row {
            DriveRow::Hotlist(hot) => Some(hot.missing.clone()),
            DriveRow::Device(_) | DriveRow::Separator => None,
        })
    }

    /// An app whose hotlist holds one entry pointing nowhere.
    fn app_with_a_missing_hotlist_entry() -> App {
        let mut app = app_with(&["a"]);
        app.hotlist
            .adopt(vec![crate::devices::hotlist::HotlistEntry {
                label: "gone".to_string(),
                path: GONE.to_string(),
            }]);
        app
    }

    /// **The finding this module was rewritten for.** One hotlist entry on a
    /// hung mount must not be able to stop the thread that draws, and the
    /// evidence that it cannot is that the popup is on screen before anything
    /// has been asked of the filesystem: a row whose directory does not exist
    /// is still ungreyed, because nobody has looked yet.
    #[test]
    fn service_drives_pushes_the_popup_without_stating_a_single_entry() {
        let mut app = app_with_a_missing_hotlist_entry();
        app.request_drives(DrivesRequest::Hotlist);

        let probe = app.service_drives().expect("the reading is handed back");
        assert!(
            matches!(probe, DrivesProbe::Open { .. }),
            "the stat and the mount table are the probe's, not this thread's"
        );
        assert!(app.dialog_is_open(), "and the popup is up on this frame");

        let rows = popup_rows(&mut app);
        assert_eq!(
            missing(&rows),
            Some(None),
            "a path that is not there is not yet known to be missing, \
             which is only possible if no stat was made"
        );
    }

    /// And the answer arrives through the channel the event loop selects on,
    /// which is where the greying finally comes from.
    #[tokio::test]
    async fn the_stat_arrives_as_an_event_and_greys_the_row() {
        let mut app = app_with_a_missing_hotlist_entry();
        app.request_drives(DrivesRequest::Hotlist);
        let probe = app.service_drives().expect("the reading is handed back");

        let (tx, mut rx) = mpsc::channel::<DrivesEvent>(1);
        probe_drives(probe, tx).await;
        let event = rx.recv().await.expect("the probe answers");
        app.apply_drives_event(event);

        let rows = popup_rows(&mut app);
        let reason = missing(&rows)
            .expect("the popup still has its hotlist row")
            .expect("and the row is now known to be missing");
        assert!(
            reason.contains("No such file") || reason.contains("not a directory"),
            "greyed with the reason the filesystem gave: {reason}"
        );
    }

    /// An answer for a popup the user has already closed changes nothing,
    /// the way a stale [`crate::app::VfsEvent`] does.
    #[tokio::test]
    async fn an_answer_for_a_popup_that_has_gone_is_dropped() {
        let mut app = app_with_a_missing_hotlist_entry();
        app.request_drives(DrivesRequest::Hotlist);
        let probe = app.service_drives().expect("the reading is handed back");
        app.close_dialogs();

        let (tx, mut rx) = mpsc::channel::<DrivesEvent>(1);
        probe_drives(probe, tx).await;
        let event = rx.recv().await.expect("the probe answers");
        app.apply_drives_event(event);

        assert!(!app.dialog_is_open(), "no popup is raised behind the user");
    }

    /// The re-enumeration is a probe as well, and it is not asked for while
    /// one is still out - but the deadline still moves, or the loop would
    /// never sleep again.
    #[test]
    fn a_poll_while_a_probe_is_unanswered_moves_the_deadline_and_asks_nothing() {
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        app.request_drives(DrivesRequest::Devices(Side::Left));
        let _probe = app.service_drives().expect("the reading is handed back");
        let due = app.drives_deadline().expect("the popup armed the deadline");

        assert!(
            app.service_drives_poll(due, true).is_none(),
            "nothing is asked while the last answer is still out"
        );
        let moved = app.drives_deadline().expect("still armed");
        assert!(moved > due, "and the deadline is in the future again");

        assert!(
            app.service_drives_poll(moved, false).is_some(),
            "with nothing outstanding the re-enumeration is asked for"
        );
    }

    /// A dirty hotlist is handed over once, and the book is clean afterwards:
    /// the write is the event loop's to spawn, not this thread's to perform.
    #[test]
    fn the_hotlist_write_is_handed_over_once() {
        let mut app = app_with(&["a"]);
        assert!(app.take_hotlist_write().is_none(), "nothing has changed");

        app.add_to_hotlist("here".to_string(), "/srv/media".to_string());
        let entries = app.take_hotlist_write().expect("the change needs writing");
        assert_eq!(entries.len(), 1);
        assert!(
            app.take_hotlist_write().is_none(),
            "and it is not queued again on the next frame"
        );
    }
}
