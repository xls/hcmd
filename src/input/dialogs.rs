//! What a dialog's answer means.
//!
//! A dialog consumes all input while it is on the stack, and it is handed a
//! key and nothing else: it has no `&mut App` and no way to act. So an answer
//! travels out as a [`DialogResult`] and lands here, in one `match` on the
//! [`DialogId`], which is why adding a dialog is adding an id and an arm
//! rather than string handling spread across the tree.
//!
//! # Answering is not the same as leaving
//!
//! the design ask their questions through a channel the connect
//! task is waiting on, and dropping that channel is a refusal. Every way out
//! of such a dialog other than answering therefore has to drop it, which is
//! why the accept path pops with
//! [`crate::app::App::pop_answered_dialog`] and every other path does not.

use crate::app::App;
use crate::config::keymap::{KeyContext, Resolution};
use crate::dialog::{DialogKey, DialogOutcome, DialogResult};
use crate::error::Result;
use crate::input::files;
use crate::input::{Action, DialogId, Focus, KeyCode, KeyPress, run_action};
use crate::ops::{JobId, JobKind, JobSpec, MaskMode};
use crate::panel::mask;
use crate::ui::dialog::JobAction;

/// Route one key into the dialog on top of the stack.
///
/// The keymap is resolved first so a user's `[dialog]` bindings win, and the
/// raw key travels alongside it so text still types - the two halves
/// [`DialogKey`] carries.
pub(super) fn dialog_key(app: &mut App, press: KeyPress, ctx: KeyContext) -> Result<()> {
    let action = match app.keymap.resolve(ctx, press) {
        Resolution::Action(action) => Some(action),
        // A chord's first half means nothing in a dialog; it is swallowed with
        // everything else rather than arming a chord the dialog cannot finish.
        Resolution::ChordPending | Resolution::Unbound => None,
    };
    // "F1 in a dialog explains that dialog." Answered before the
    // dialog is handed the key, because no dialog has an arm for it and every
    // one of them would swallow it - which would leave the reference's whole
    // `Dialogs` section unreachable by the key that names it.
    if action == Some(Action::Help) {
        // On top of the dialog, not behind it. This used to open the whole
        // reference in a viewer, and dialogs draw over viewers, so the answer
        // appeared underneath the question that prompted it. `Esc` closes the
        // explanation and leaves the reader on the dialog they asked about.
        let id = app.top_dialog().map(crate::dialog::Dialog::id);
        if let Some(id) = id {
            let (title, body) = crate::ui::help::dialog_help_text(id);
            // A message box, which is what every other multi-line answer in
            // this program already is. It takes its width from the longest
            // line and its height from how many there are, so the paragraph is
            // laid out rather than folded into a corner.
            //
            // The file-information box was used here first and was the wrong
            // shape: it measures its height in *rows of facts*, and a wrapped
            // paragraph is one row, so a whole page of text arrived in a
            // two-line window.
            app.push_dialog(Box::new(crate::dialog::MessageDialog::new(
                title,
                body.lines().map(str::to_string).collect(),
            )));
        }
        return Ok(());
    }
    let key = DialogKey { press, action };

    let Some(dialog) = app.top_dialog_mut() else {
        // The stack emptied under us. Restore focus rather than sitting in a
        // `Focus::Dialog` with nothing behind it.
        app.set_focus(Focus::Panel(app.active_side));
        return Ok(());
    };
    let outcome = dialog.handle_key(&key);

    match outcome {
        DialogOutcome::Consumed | DialogOutcome::Ignored => {}
        DialogOutcome::Cancel => {
            // The theme picker previews as it moves, so cancelling it has to
            // put back what was running when it opened - otherwise Esc leaves
            // the screen wearing whatever the cursor last passed over, which
            // is the opposite of what Esc means.
            let restore = app
                .top_dialog()
                .filter(|d| d.id() == crate::input::DialogId::Theme)
                .and_then(|d| d.as_any())
                .and_then(|any| any.downcast_ref::<crate::ui::dialog::theme::ThemeDialog>())
                .map(|picker| picker.original().to_string());
            app.pop_dialog();
            if let Some(name) = restore {
                app.restore_theme(&name);
            }
        }
        DialogOutcome::Accept(result) => {
            // The job the dialog was a view of goes with the answer: a conflict
            // question belongs to one job, and by the time the answer is acted
            // on the dialog is off the stack.
            // Answered, so the pending `oneshot` of the design
            // must survive the pop: see `App::pop_answered_dialog`.
            // The dialog itself is kept, not merely its id: the host form can
            // carry a password, and a secret typed into a form has exactly one
            // moment to be read - after the form is answered and before it is
            // dropped. It never reaches `hosts.toml`.
            let closed = app.pop_answered_dialog();
            if let Some(dialog) = closed {
                let (id, job) = (dialog.id(), dialog.job());
                let secret = dialog
                    .as_any()
                    .and_then(|any| any.downcast_ref::<crate::remote::connect::HostFormDialog>())
                    .and_then(crate::remote::connect::HostFormDialog::typed_password);
                drop(dialog);
                if let Some(secret) = secret {
                    app.pending_host_secret = Some(secret);
                }
                dialog_answered(app, id, job, result);
            }
        }
        DialogOutcome::Push(next) => app.push_dialog(next),
        DialogOutcome::Replace(next) => {
            app.pop_dialog();
            app.push_dialog(next);
        }
    }
    Ok(())
}

/// Act on a dialog's answer.
///
/// **This is the extension point.** A new dialog adds a [`DialogId`] variant
/// and an arm here; nothing else in `dispatch` has to change, and the dialog
/// itself stays free of `&mut App`.
///
/// The catch-all is deliberate and must stay: v0.2 lands four more dialogs from
/// three agents, and an exhaustive match here would make every one of them a
/// merge conflict.
pub fn dialog_accepted(app: &mut App, id: DialogId, result: DialogResult) {
    dialog_answered(app, id, None, result);
}

/// [`dialog_accepted`], told which job the dialog was a view of.
///
/// Only the conflict dialog needs it, and it needs it absolutely: the design
/// lets a **backgrounded** job sit parked on a question while a second job's
/// dialog is the one on screen, so "the first job with a pending decision" and
/// "the job the user was looking at" are different jobs.
pub fn dialog_answered(app: &mut App, id: DialogId, job: Option<JobId>, result: DialogResult) {
    match (id, &result) {
        // The theme is already on screen: the picker previewed it on the way
        // here, so accepting is keeping it. Accepting also writes it into
        // `config.toml`, because a picker whose choice is forgotten at exit is
        // a preview and not a setting.
        //
        // A write that fails is reported and nothing else: the theme is still
        // on screen and still correct for this session, so refusing the
        // choice over a read-only config would take away the part that worked.
        //
        // A name the picker offered with a `+` after it is one the repository
        // has and this machine does not, and choosing it is what fetches it.
        // That is the one case where accepting is not simply keeping what is
        // on screen: it was never previewed, because there was nothing to
        // preview, so it is downloaded, applied and then saved. A download
        // that fails leaves the session exactly as it was and says so.
        // The picker names a built-in template, or the row that removes the
        // applied one. Nothing is fetched and nothing is saved: a template is
        // a way of reading the file that is open, not a setting, and it goes
        // away with the viewer that it was chosen for.
        (DialogId::Template, DialogResult::Text(name)) => {
            let wanted = name.clone();
            let Some(viewer) = app.viewer_mut() else {
                return;
            };
            if wanted == crate::ui::dialog::template::NONE {
                viewer.set_template(None);
                app.message = Some("template: none".to_string());
                return;
            }
            let Some(template) = crate::viewer::fileinfo::builtin()
                .iter()
                .find(|t| t.name == wanted)
            else {
                app.message = Some(format!("template: {wanted} - no such template"));
                return;
            };
            viewer.set_template(Some(template.clone()));
            // Where it lands, not just which one it is: a template is applied
            // at the cursor, and someone who pressed the key at offset 0x40
            // needs to be told that is where the header is being read from
            // rather than discover it from the colouring.
            let at = viewer.cursor();
            let fields = template.fields.len();
            app.message = Some(format!(
                "template: {wanted} - {fields} fields from offset {at:#x}"
            ));
        }
        (DialogId::Theme, DialogResult::Text(name)) => {
            if let Err(err) = crate::config::catalogue::ensure_installed(name) {
                app.message = Some(format!("theme: {name} - could not fetch it: {err}"));
                return;
            }
            if !app.adopt_theme(name) {
                app.message = Some(format!("theme: {name} - nothing here reads as that theme"));
                return;
            }
            app.message = Some(match crate::config::persist::store_theme(name) {
                Ok(path) => format!("theme: {name} - saved in {}", path.display()),
                Err(err) => {
                    format!("theme: {name} for this session only - could not save it: {err}")
                }
            });
        }
        (DialogId::GotoPath, DialogResult::Text(raw)) => {
            let side = app.active_side;
            let base = app.active_panel().active_tab().path.clone();
            match base.local_path() {
                // Local, and `resolve` also proves the directory exists before
                // the panel is sent anywhere.
                Some(here) => match crate::panel::goto::resolve(raw, Some(here)) {
                    Ok(target) => app.navigate(side, crate::vfs::VfsPath::local(target)),
                    Err(why) => app.message = Some(why),
                },
                // Anywhere else - a remote, an archive, a search listing - the
                // typed path belongs to the backend the panel is already on.
                // It used to be handed to `VfsPath::local` unconditionally, so
                // `Ctrl+G` on a connected panel walked out of the connection
                // and into the local filesystem without saying so. Existence
                // is not checked here because `resolve` checks it with
                // `std::fs`, which is the wrong filesystem; the read that
                // follows reports a path that is not there.
                None => match crate::panel::goto::expand(raw, Some(base.tail())) {
                    Ok(target) => app.navigate(side, base.with_tail(target)),
                    Err(why) => app.message = Some(why),
                },
            }
        }
        // `Ctrl+G` inside the viewer. An offset that will not
        // parse is reported and the position is not moved - never a guess.
        (DialogId::GotoOffset, DialogResult::Text(raw)) => {
            // An offset past the end is **refused with the size**, not clamped
            // to it: `Ctrl+G` is a statement about a byte, and silently landing
            // somewhere else would be a wrong answer that looked like a right
            // one. A file whose size is not yet known cannot be refused
            // against, so it is accepted (`resolve_goto` holds that rule).
            let len = app.focused_viewer().and_then(crate::viewer::Viewer::len);
            match crate::viewer::hex::resolve_goto(raw, len) {
                Ok(target) => {
                    // A percentage and a line number are never refused against
                    // the size: the viewer answers both approximately while the
                    // index is still running, which is the rule for
                    // exactly these two.
                    let outcome = app.focused_viewer_mut().map(|v| match target {
                        crate::viewer::hex::GotoTarget::Offset(at) => v.goto_offset(at),
                        crate::viewer::hex::GotoTarget::Percent(p) => v.goto_percent(p),
                        crate::viewer::hex::GotoTarget::Line(n) => v.goto_line(n),
                    });
                    if let Some(Err(err)) = outcome {
                        app.message = Some(err.to_string());
                    }
                }
                Err(err) => app.message = Some(err.to_string()),
            }
        }
        (DialogId::ConfirmEditLarge, DialogResult::Confirm(true)) => {
            // The retried `F4` opens the editor rather than asking again: the
            // cursor has not moved, so the file is the same one.
            if app.editor_size_pending.take().is_some() {
                app.editor_size_confirmed = true;
                crate::input::files::open_editor(app);
            }
        }
        (DialogId::ConfirmEditLarge, DialogResult::Confirm(false)) => {
            app.editor_size_pending = None;
        }
        (DialogId::Symlink | DialogId::Hardlink, DialogResult::Text(name)) => {
            let symbolic = id == DialogId::Symlink;
            let base = app.active_panel().active_tab().path.clone();
            let sources = std::mem::take(&mut app.draft.sources);
            let trimmed = name.trim().to_string();
            match sources.first().cloned() {
                // The link's name is one component, not a path: a name with a
                // separator would put the link somewhere the dialog did not
                // say, which is the rule an archive member's name follows too.
                Some(_) if trimmed.contains('/') || trimmed == ".." || trimmed == "." => {
                    app.message = Some(format!("{trimmed}: a link's name is one component"));
                }
                Some(target) if !trimmed.is_empty() => {
                    app.request_link(crate::app::links::LinkRequest {
                        target,
                        link: base.join(&trimmed),
                        symbolic,
                    });
                }
                _ => app.message = Some("a link needs a name".to_string()),
            }
        }
        (DialogId::Permissions, DialogResult::Text(text)) => {
            let sources = std::mem::take(&mut app.draft.sources);
            match u32::from_str_radix(text.trim(), 8) {
                // `0o7777` is the mode bits and the three set-id/sticky bits;
                // anything above that is not a mode and is more likely a
                // decimal number typed into an octal field.
                Ok(mode) if mode <= 0o7777 && !sources.is_empty() => {
                    app.request_chmod(crate::app::links::ChmodRequest {
                        paths: sources,
                        mode,
                    });
                }
                Ok(_) => {
                    app.message = Some(format!("{text}: not a mode; try 644, 755 or 600"));
                }
                Err(_) => {
                    app.message = Some(format!("{text}: a mode is octal digits, like 644"));
                }
            }
        }
        (DialogId::Split, DialogResult::Text(size)) => {
            let sources = std::mem::take(&mut app.draft.sources);
            let target = app.draft.target.take();
            match crate::config::ByteSize::parse(size.trim()) {
                Ok(part) if part.bytes() > 0 && !sources.is_empty() => {
                    let mut spec = JobSpec::new(JobKind::Split, sources, target);
                    spec.options.part_size = part.bytes();
                    app.request_job(spec);
                }
                Ok(_) => app.message = Some("a part size of zero splits nothing".to_string()),
                Err(err) => app.message = Some(format!("{size}: {err}")),
            }
        }
        (DialogId::Checksum, DialogResult::Text(name)) => {
            let base = app.active_panel().active_tab().path.clone();
            let sources = std::mem::take(&mut app.draft.sources);
            if sources.is_empty() || name.trim().is_empty() {
                app.message = Some("nothing to checksum".to_string());
            } else {
                app.request_job(JobSpec::new(
                    JobKind::Checksum { verify: false },
                    sources,
                    Some(base.join(name.trim())),
                ));
            }
        }
        (DialogId::Mkdir, DialogResult::Text(name)) => {
            let base = app.active_panel().active_tab().path.clone();
            // Land the cursor on what was just created once the panel re-reads
            // (the `pending_select`, added for exactly this shape of
            // problem). `a/b/c` creates three levels but only `a` appears in
            // this listing, so the first component is what to look for.
            let first = name
                .trim_matches('/')
                .split('/')
                .next()
                .unwrap_or(name.as_str());
            if !first.is_empty() {
                let side = app.active_side;
                app.panel_mut(side).active_tab_mut().pending_select = Some(first.to_string());
            }
            let spec = JobSpec::new(JobKind::Mkdir, Vec::new(), Some(base.join(name)));
            app.request_job(spec);
        }
        // The copy/move dialog's `+ F7`: "A `+ F7` button
        // creates a new directory **for the target**." Not for the active
        // panel - with `F5` the target is the *other* panel, so sharing the
        // plain `F7` arm created the directory in the source directory, left
        // the target field pointing at the old destination, and moved the
        // source panel's cursor onto it.
        (DialogId::MkdirForTarget, DialogResult::Text(name)) => {
            files::mkdir_for_target(app, name);
        }
        // `F2` / `Shift+F6`. The dialog has already refused an
        // empty name, a separator and a name that is taken.
        (DialogId::Rename, DialogResult::Text(name)) => {
            files::rename_accepted(app, name);
        }
        // `Shift+F4`: "prompts for a name, creates the file,
        // then opens the editor". The creation is the event loop's - `dispatch`
        // may not touch the filesystem - so this
        // names the file and queues both halves.
        (DialogId::EditNew, DialogResult::Text(name)) => {
            files::edit_new_accepted(app, name);
        }
        // `+` / `-`. The mask is remembered for the session
        // whichever direction it was typed in, because the next `+` after a
        // `-` wants the same one.
        (DialogId::SelectMask | DialogId::UnselectMask, DialogResult::Text(mask)) => {
            // The prompt encodes its `Exclude directories` checkbox onto the
            // front of its answer, because `DialogResult::Text` carries one
            // string. the design makes both sticky for the session.
            let (mask, exclude_dirs) = mask::decode_answer(mask);
            let mask = mask.trim().to_string();
            app.masks.last = mask.clone();
            app.masks.exclude_dirs = exclude_dirs;
            let mark = id == DialogId::SelectMask;
            let tab = app.active_panel_mut().active_tab_mut();
            app.message = Some(
                match mask::apply(tab, &mask, MaskMode::Wildcard, mark, exclude_dirs) {
                    Ok(outcome) => outcome.message(&mask, mark),
                    // Unreachable while the prompt refuses to switch modes, and
                    // still not a panic if that ever changes.
                    Err(why) => why.to_string(),
                },
            );
        }
        // `F5` / `F6`. The target field is a path and a mask;
        // the mask half is `Only files of this type`'s sibling and does not
        // rename anything yet -.
        (DialogId::CopyMove, DialogResult::CopyMove(answer)) => {
            files::copy_move_accepted(app, answer);
        }
        // `synchronize`. The dry run was the dialog; the plan the user left it
        // with becomes the copy and delete jobs, queued together.
        (DialogId::Synchronize, DialogResult::Synchronize(plan)) => {
            files::synchronize_accepted(app, plan);
        }
        // `Alt+F5`. The archive does not exist yet and making
        // one is filesystem work, so what the dialog leaves behind is the
        // request; the event loop creates it and then queues the copy, for the
        // same reason `F3` queues a viewer rather than opening one.
        (DialogId::Pack, DialogResult::Pack(answer)) => {
            files::pack_accepted(app, answer);
        }
        // `Shift+R`. The dialog collects the settings and nothing else; the
        // job reads the images, and the conflict dialog answers for the names
        // already in the way.
        (DialogId::Resize, DialogResult::Resize(settings)) => {
            files::resize_accepted(app, settings);
        }
        // "This file already exists". The answer goes to the
        // job the dialog was built for and to no other: with one job
        // backgrounded and parked and another asking in the foreground,
        // answering "whichever is parked first" overwrites a file that was
        // never named on screen - and an "apply to all" installs a standing
        // policy in a batch the user was not looking at.
        (DialogId::Conflict, DialogResult::Conflict(decision)) => {
            let target = job.filter(|id| {
                app.job(*id)
                    .is_some_and(|status| status.pending_decision.is_some())
            });
            match target {
                Some(id) => {
                    app.answer_job(id, (**decision).clone());
                }
                None => app.message = Some("that operation has already finished".to_string()),
            }
        }
        // the warning between `rewrite_warn_size` and
        // `rewrite_max_size`, answered. `Esc` is a `Confirm(false)` like every
        // other confirmation, so cancelling by any route reaches the same arm
        // and the held job is never left in limbo.
        (DialogId::ConfirmRewrite, DialogResult::Confirm(answer)) => {
            app.resume_rewrite_gate(*answer);
        }
        // `F10` / `Alt+Q` / `Alt+F4`.
        (DialogId::ConfirmQuit, DialogResult::Confirm(answer)) => app.should_quit = *answer,
        // `F8` / `Shift+F8`.
        (DialogId::ConfirmDelete, DialogResult::Confirm(true)) => {
            files::delete_confirmed(app);
        }
        (DialogId::ConfirmDelete, DialogResult::Confirm(false)) => {
            // `No` is not merely "do nothing": the operands captured for the
            // prompt go with it, so nothing can act on them later.
            app.draft.discard();
        }
        // The three job dialogs answer with a `JobAction` in a `Text`, because
        // `DialogResult` has no job variant.
        (
            DialogId::Progress | DialogId::JobQueue | DialogId::JobSummary,
            DialogResult::Text(text),
        ) => match JobAction::parse(text) {
            Some(action) => files::run_job_action(app, action),
            None => app.message = Some(format!("{text}: not a job action")),
        },
        // `Start search`. The query is already valid - the
        // dialog refuses rather than handing back one that cannot compile -
        // and everything the search needs from the configuration is stamped on
        // by `App::request_search`.
        (DialogId::Find, DialogResult::Find(answer)) => {
            super::search::find_accepted(app, answer);
        }
        // The `Save as…` prompt on top of the Find dialog.
        (DialogId::SaveSearch, DialogResult::Text(name)) => {
            super::search::save_search_named(app, name);
        }
        // `Ctrl+M`'s action row.
        (DialogId::MultiRename, DialogResult::MultiRename(answer)) => {
            super::search::multi_rename_accepted(app, answer);
        }
        // `Ctrl+F`. Connecting is I/O, so the answer is queued
        // and the event loop starts it, exactly as a search is.
        (DialogId::Connect, DialogResult::Connect(answer)) => {
            app.connect_answered(answer.clone());
        }
        // `F4` or `F8` edited the host book and then the dialog was closed
        // without connecting: the edit is still the user's and is still saved.
        (DialogId::Connect, DialogResult::Hosts(list)) => {
            app.hosts.replace(list.clone());
        }
        // The Add-host form answered: the list goes back into the connect
        // dialog underneath it, which is exactly the `save_search_named`
        // shape.
        (DialogId::HostForm, DialogResult::Hosts(list)) => {
            host_form_answered(app, list.clone());
        }
        // the unknown host key. `Cancel` and `Esc` both arrive
        // here as `Confirm(false)`, and both refuse.
        (DialogId::HostKey, DialogResult::Confirm(accepted)) => {
            app.answer_host_key(*accepted);
        }
        // the changed key: a message, with nothing to accept. The
        // connection was already aborted before it was drawn.
        (DialogId::HostKeyChanged, _) => {}
        // the password or passphrase.
        (DialogId::RemoteSecret, DialogResult::Secret(answer)) => {
            app.answer_secret(Some((**answer).clone()));
        }
        // `Ctrl+F` on a connected panel.
        (DialogId::ConfirmDisconnect, DialogResult::Confirm(true)) => {
            let side = app.active_side;
            let tab = app.panel(side).active_index();
            app.disconnect(side, tab);
        }
        (DialogId::ConfirmDisconnect, DialogResult::Confirm(false)) => {}
        // the opt-in before a content search crosses a network.
        (DialogId::ConfirmRemoteSearch, DialogResult::Confirm(allowed)) => {
            app.answer_remote_search(*allowed);
        }
        // ---------------------------------------- the design devices ----
        // The popup answers a **path**, and which panel it changes is the
        // popup's own question, not the focused panel's: `Alt+F1` is spatial,
        // so `Drive(Left)` navigates the left panel and moves focus there
        // whichever panel was active when the key was pressed (invariant I1).
        (DialogId::Drive(side), DialogResult::Text(path)) => {
            app.navigate(side, crate::vfs::VfsPath::local(path));
            app.set_focus(Focus::Panel(side));
        }
        // `Ctrl+D` acts on the **active** panel, which is
        // where focus already is.
        (DialogId::Hotlist, DialogResult::Text(path)) => {
            app.navigate(app.active_side, crate::vfs::VfsPath::local(path));
        }
        // `Ctrl+Shift+D`'s label. The path is the active panel's, which has
        // not moved: the prompt is modal (see `open_hotlist_add`).
        (DialogId::HotlistAdd, DialogResult::Text(label)) => {
            let path = app.active_panel().active_tab().path.clone();
            match path.local_path() {
                Some(local) => {
                    let path = local.to_string_lossy().into_owned();
                    app.add_to_hotlist(label.clone(), path);
                }
                None => {
                    app.message = Some(
                        "the hotlist holds local directories; this panel is not showing one"
                            .to_string(),
                    );
                }
            }
        }

        // ------------------------------------------- the design menus ---
        // A menu item is an action id, and running it is running that action:
        // the design makes every item "a key that already exists", so the
        // menu is a second route to the same code and never a second copy of
        // it (invariant I11).
        (DialogId::Menu, DialogResult::Text(text)) => match Action::from_id(text) {
            Some(action) => {
                if let Err(err) = run_action(app, action, KeyPress::plain(KeyCode::Null)) {
                    app.message = Some(err.to_string());
                }
            }
            // Not reachable from `menu::model`, which builds every row from an
            // `Action`. Said rather than ignored, because a silent no-op on a
            // menu the user just clicked is the worst of the three outcomes.
            None => app.message = Some(format!("{text}: not an action")),
        },

        // ---------------------------------------- the design context --
        (DialogId::ContextMenu, DialogResult::Text(text)) => {
            match crate::ui::dialog::ContextChoice::parse(text) {
                Some(choice) => super::run_context_choice(app, choice),
                None => app.message = Some(format!("{text}: not a context menu answer")),
            }
        }

        // ----------------------------------------- the design execute -
        (DialogId::Execute, DialogResult::Text(text)) => {
            let subject = app.take_open_subject();
            match (crate::ui::dialog::ExecuteChoice::parse(text), subject) {
                (Some(crate::ui::dialog::ExecuteChoice::Execute), Some(path)) => {
                    // Back onto the same queue rather than a second launcher
                    // written here: `App::service_open` is the one place in
                    // the program that runs anything.
                    app.request_open(crate::app::OpenRequest {
                        path,
                        never_execute: false,
                        intent: crate::app::OpenIntent::Execute,
                    });
                }
                (Some(crate::ui::dialog::ExecuteChoice::OpenWith), Some(path)) => {
                    app.request_open(crate::app::OpenRequest {
                        path,
                        never_execute: true,
                        intent: crate::app::OpenIntent::Chooser,
                    });
                }
                (Some(crate::ui::dialog::ExecuteChoice::View), Some(path)) => {
                    app.request_view(crate::app::ViewRequest::File { path, at: None });
                }
                // Cancel, and any answer whose subject went away with a reload
                // or a closed stack: the design makes Cancel the default,
                // so doing nothing is the right answer to both.
                (Some(crate::ui::dialog::ExecuteChoice::Cancel) | None, _) | (Some(_), None) => {}
            }
        }

        // ----------------------------------------- the design chooser -
        (DialogId::OpenWith, DialogResult::Text(app_id)) => {
            if let Some(path) = app.take_open_subject() {
                app.request_open(crate::app::OpenRequest {
                    path,
                    never_execute: true,
                    intent: crate::app::OpenIntent::Application(app_id.clone()),
                });
            }
        }

        // ------------------------------------------ the design history --
        // The chosen command is **put on** the command line, not run: `Enter`
        // there is what runs one.
        (DialogId::History, DialogResult::Text(command)) => {
            app.cmdline.set_text(command);
            app.set_focus(Focus::CommandLine);
        }

        // The result list is a report; closing it is the whole of its answer.
        (DialogId::RenameResult, _) => {}
        (DialogId::Message | DialogId::JobSummary, _) => {}
        _ => {
            // An answer nobody has wired up yet is a bug in the wiring, not in
            // the dialog, and saying so beats a silent no-op.
            app.message = Some(format!(
                "{}: nothing is wired up to act on this dialog yet",
                id.id()
            ));
        }
    }
}

/// The Add-host form answered.
///
/// The list goes back into the connect dialog underneath, which is still on
/// the stack, and is marked for writing. Exactly the shape `save_search_named`
/// has for the Find dialog's Load/Save tab.
fn host_form_answered(app: &mut App, list: Vec<crate::remote::hosts::SavedHost>) {
    // A password typed into the form goes into the **keyring**, under the same
    // account the connect path reads it back from, and the host's auth method
    // becomes `keyring` so that it is looked for. `hosts.toml` never carries
    // it: that file is the non-secret half and stays that way even now the
    // form can ask.
    //
    // Queued rather than written here, because a keyring write is I/O and
    // `dispatch` performs none.
    if let Some(secret) = app.pending_host_secret.take()
        && let Some(host) = list.last()
    {
        app.request_keyring_write(crate::app::links::KeyringWrite {
            account: host.target().keyring_account(),
            secret,
        });
    }
    app.hosts.replace(list.clone());
    if let Some(dialog) = app
        .top_dialog_mut()
        .and_then(|d| d.as_any_mut())
        .and_then(|any| any.downcast_mut::<crate::remote::connect::ConnectDialog>())
    {
        dialog.set_hosts(list);
    }
}
