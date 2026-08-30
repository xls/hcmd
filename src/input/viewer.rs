//! Keys that belong to a viewer.
//!
//! A viewer consumes all input while it has the screen, so this is where a key
//! ends up once [`super::dispatch`] has decided that the viewer is the one
//! consumer. The same handlers serve both kinds: the full-screen viewer, which
//! holds [`Focus::Viewer`], and the quick view inside a panel, which does not
//! and whose panel still holds [`Focus::Panel`].
//!
//! # One table for movement
//!
//! The ten movements that can be extended with `Shift` are declared once, in
//! [`VIEWER_MOTIONS`], so that the function which reads the modifiers and the
//! function which performs the action cannot disagree about which actions are
//! movements.

use crate::app::App;
use crate::config::Keymap;
use crate::config::keymap::{KeyContext, Resolution};
use crate::dialog::InputDialog;
use crate::error::Result;
use crate::input::{Action, DialogId, Focus, KeyCode, KeyModifiers, KeyPress, run_action};
use crate::viewer::select::{Extend, Motion};

/// `F3`: view the entry under the cursor.
///
/// A directory is refused rather than opened as bytes, and so is `..`. Anything
/// else goes to the viewer - including a file that turns out to be binary,
/// which the design opens in hex rather than declining.
pub(super) fn open_viewer(app: &mut App) {
    let tab = app.active_panel().active_tab();
    let Some(entry) = tab.current() else {
        app.message = Some("nothing to view".to_string());
        return;
    };
    if entry.is_parent || entry.is_dir() {
        app.message = Some(format!("{}: is a directory", entry.name));
        return;
    }
    let path = entry
        .location
        .clone()
        .unwrap_or_else(|| tab.path.join(&entry.name));
    // "`F3` on a remote file streams into the viewer with a
    // size cap (`remote.view_max_size`, default 32 MB) before it offers to
    // download instead." Decided here, before anything is read, because the
    // whole point of the cap is not to have started the transfer.
    let cap = app.config.remote.view_max_size.bytes();
    if crate::remote::RemoteId::from_path(&path).is_some() && entry.size > cap {
        app.message = Some(format!(
            "{}: {} is over the {} remote.view_max_size cap - copy it here with F5 first",
            entry.name,
            crate::panel::format::human_size(entry.size),
            crate::panel::format::human_size(cap)
        ));
        return;
    }
    app.request_view(crate::app::ViewRequest::File { path, at: None });
}

/// Route one key into a quick view that has focus.
///
/// Almost everything goes to the viewer, which is the whole of "the viewer's
/// own keys apply". Four keys stay the panel's, because a quick view is a
/// *panel* showing a file and there has to be a way out of it: `Tab` moves
/// focus back to the other panel, `Ctrl+Q` closes the quick view, `F1` is the
/// whole-program reference, and `F10` quits. Without them the viewer would
/// swallow all four - it has no use for any of them - and the only way out of
/// a quick view would be to kill the program.
pub(super) fn quick_view_key(app: &mut App, press: KeyPress) -> Result<()> {
    if let Resolution::Action(action) = app.keymap.resolve(KeyContext::Panel, press)
        && matches!(
            action,
            Action::OtherPanel | Action::QuickView | Action::Help | Action::Quit
        )
    {
        return run_action(app, action, press);
    }
    viewer_key(app, press, KeyContext::Viewer)
}

/// Route one key into the viewer.
///
/// The keymap is resolved against the `viewer` context, so the design's
/// order is unchanged and a user's `[viewer]` bindings win. What differs from
/// a panel is the ending: a key that resolves to nothing, or to an action the
/// viewer has no use for, is **swallowed**. The viewer consumes all input, and
/// there is nothing behind it that a fall-through could reach.
pub(super) fn viewer_key(app: &mut App, press: KeyPress, ctx: KeyContext) -> Result<()> {
    // the find bar "behaves like the panel quick search" -
    // typing searches immediately. So while it is open the printable keys are
    // *text*, not actions: `n` types an `n` rather than stepping, and `q` does
    // not close the viewer out from under a half-typed pattern. Everything
    // else - the function keys, the control keys - still resolves normally,
    // which is what keeps `F8` and `Ctrl+G` reachable mid-search.
    if app.focused_viewer().is_some_and(|v| v.find().is_open()) && find_bar_key(app, press)? {
        return Ok(());
    }
    // the "`Shift` + any of those". A key that resolves as itself
    // runs as itself; only a shifted press that resolves to nothing is asked
    // whether the unshifted key is a movement.
    let Some((action, extend)) = viewer_extend(&app.keymap, ctx, press) else {
        // A chord's first half means nothing here, for the same reason it means
        // nothing in a dialog: the viewer would have to hold it, and nothing
        // shipped is a chord.
        return Ok(());
    };
    // "`Esc` clears the selection; **if there is none, close the
    // viewer**." Answered here, where the press is still in hand, because the
    // rule is about the *key* and not about the action: `F3` and `q` resolve to
    // the same `Close` and shut the viewer outright, which is
    // the judgement call.
    if action == Action::Close
        && press.code == KeyCode::Esc
        && app
            .viewer_mut()
            .is_some_and(crate::viewer::Viewer::clear_selection)
    {
        app.message = Some("selection cleared".to_string());
        return Ok(());
    }
    viewer_action(app, action, extend)
}

/// The ten actions a viewer movement key can resolve to, paired with what they
/// do to the cursor.
///
/// One table, so [`viewer_extend`] and `viewer_action` cannot disagree about
/// what counts as a movement: a key that extends a selection and a key that
/// moves the cursor have to be the same set, or `Shift+Home` would extend
/// something `Home` does not move.
pub const VIEWER_MOTIONS: &[(Action, Motion)] = &[
    (Action::CursorUp, Motion::Up),
    (Action::CursorDown, Motion::Down),
    (Action::CaretLeft, Motion::Left),
    (Action::CaretRight, Motion::Right),
    (Action::CursorPageUp, Motion::PageUp),
    (Action::CursorPageDown, Motion::PageDown),
    (Action::LineStart, Motion::RowStart),
    (Action::LineEnd, Motion::RowEnd),
    (Action::CursorTop, Motion::FileStart),
    (Action::CursorBottom, Motion::FileEnd),
];

/// The motion an action moves the cursor by, or `None` when it is not a
/// movement at all.
pub fn viewer_motion(action: Action) -> Option<Motion> {
    VIEWER_MOTIONS
        .iter()
        .find(|(a, _)| *a == action)
        .map(|(_, m)| *m)
}

/// the "`Shift` + any of those" and "`Ctrl+Shift` + it".
///
/// Returns the action the press is asking for and how it extends the
/// selection. The order is the design and it is the whole of
/// the rule:
///
/// 1. the keymap resolves the press as it stands. **A user binding always
///    wins** and the order is untouched; the answer extends nothing.
/// 2. otherwise, with `Shift` held, the press without `Shift` is resolved. A
///    movement there is that movement, extending **linearly**.
/// 3. otherwise, with `Shift` and `Ctrl` held, the press without either is
///    resolved. A movement there extends **rectangularly**.
/// 4. otherwise `None`, and the key is swallowed as any unbound viewer key is.
///
///
/// Step 3 is why `Ctrl+Shift+Home` comes out *linear* without a special case:
/// step 2 already finds `ctrl+home` bound to `CursorTop`, and a rectangle over
/// the whole file is the whole file anyway.
///
pub fn viewer_extend(
    keymap: &Keymap,
    ctx: KeyContext,
    press: KeyPress,
) -> Option<(Action, Extend)> {
    match keymap.resolve(ctx, press) {
        Resolution::Action(action) => return Some((action, Extend::None)),
        // A half-spelled chord is held by nobody here, so it is swallowed
        // rather than falling through to the shifted readings below - which
        // would turn `Ctrl+X` into a movement if a user ever bound one.
        Resolution::ChordPending => return None,
        Resolution::Unbound => {}
    }
    // The **normalised** modifiers, because that is where `Shift` is honestly
    // recorded: a terminal reports `Shift+N` as a bare `N` and
    // `KeyPress::normalized` is what turns that back into `n` plus `SHIFT`
    // (`src/input/binding.rs`). Asking the raw press would miss every letter.
    let mods = press.normalized().mods;
    if !mods.contains(KeyModifiers::SHIFT) {
        return None;
    }
    // `resolve` normalises what it is given, so handing it the raw code with a
    // modifier taken away asks exactly "what would this key do unshifted".
    let without = |drop: KeyModifiers| KeyPress::new(press.code, mods.difference(drop));
    if let Resolution::Action(action) = keymap.resolve(ctx, without(KeyModifiers::SHIFT))
        && viewer_motion(action).is_some()
    {
        return Some((action, Extend::Linear));
    }
    if !mods.contains(KeyModifiers::CONTROL) {
        return None;
    }
    if let Resolution::Action(action) =
        keymap.resolve(ctx, without(KeyModifiers::SHIFT | KeyModifiers::CONTROL))
        && viewer_motion(action).is_some()
    {
        return Some((action, Extend::Rectangular));
    }
    None
}

/// The find bar's own keys.
///
/// Returns true when the key was the bar's and must go no further. `Esc`
/// closes the bar **and keeps the position and the matches**, which is why it
/// is answered here rather than falling through to `[viewer] close` - inside a
/// search, `Esc` means "stop typing", not "close the file".
fn find_bar_key(app: &mut App, press: KeyPress) -> Result<bool> {
    let found = match press.code {
        // `as_text` rather than the `Char` payload, because normalisation
        // folded `X` to `x` plus `SHIFT` and only `as_text` undoes that. A
        // pattern typed in capitals has to arrive in capitals or smart case
        // could never see one - and `as_text` is `None`
        // with `Ctrl` or `Alt` held, which is what leaves `Ctrl+G` and `Ctrl+F`
        // to the keymap.
        KeyCode::Char(_) => {
            let Some(c) = press.as_text() else {
                return Ok(false);
            };
            match app.focused_viewer_mut() {
                Some(viewer) => viewer.find_type(c),
                None => return Ok(false),
            }
        }
        KeyCode::Backspace => match app.focused_viewer_mut() {
            Some(viewer) => viewer.find_backspace(),
            None => return Ok(false),
        },
        // `Enter` and `Esc` both leave the bar; the difference is only that
        // `Enter` reads as "that one" and `Esc` as "never mind", and neither
        // moves the viewer, so they do the same thing (the "closes
        // the bar and keeps position").
        KeyCode::Esc | KeyCode::Enter => {
            if let Some(viewer) = app.focused_viewer_mut() {
                viewer.close_find();
            }
            return Ok(true);
        }
        _ => return Ok(false),
    };
    // typing in the bar IS searching - it searches incrementally as you type
    // - so this is where the session's pattern is set, not only on `n` and
    // `F3`. Recorded after the keystroke so it is what the bar now holds
    // rather than what it held before.
    if let Some(query) = app.focused_viewer().map(|v| v.find_query().clone())
        && !query.input.is_empty()
    {
        app.viewers.last_find = Some(query);
    }
    report_find(app, found);
    Ok(true)
}

/// Say what a search found, when there was nothing to show for it.
///
/// A hit needs no message - the screen moved and the match is painted. The
/// other two answers are invisible and would otherwise look like a key that
/// did nothing.
fn report_find(app: &mut App, found: Result<crate::viewer::find::Found>) {
    use crate::viewer::find::Found;
    match found {
        Ok(Found::Hit(_)) => {}
        // Nothing at all, or nothing yet: the difference matters on a 40 GB
        // file, where "not in the first few megabytes" is not "not there"
        // (the honesty rule, applied to a search).
        Ok(Found::None) => {
            let what = app
                .viewer()
                .map(|v| v.find().input().to_string())
                .unwrap_or_default();
            if !what.is_empty() {
                app.message = Some(format!("{what}: not found"));
            }
        }
        // Bounded, not finished: `n` again carries on from where this stopped,
        // which is what makes the far end of a 40 GB file reachable without
        // ever letting one keystroke read it all.
        Ok(Found::Budget(at)) => {
            app.message = Some(format!(
                "no match in the first {at} bytes - n keeps looking"
            ));
        }
        Err(err) => app.message = Some(err.to_string()),
    }
}

/// `t`: choose a binary struct template for the hex dump.
///
/// Every built-in template, with the applied one under the cursor so that
/// stepping away from it and back is a comparison rather than a search, and
/// [`crate::ui::dialog::template::NONE`] first so the choice can be undone.
///
/// Offered in text mode as well as in hex. The colouring only shows in hex,
/// but refusing the key in text mode would mean the answer to "why did nothing
/// happen" is a mode the person has not thought about yet; applying it and
/// letting `2` show the result is the shorter path.
fn open_template_picker(app: &mut App) {
    let current = app
        .viewer()
        .and_then(crate::viewer::Viewer::template_name)
        .map(str::to_string);
    let mut names = vec![crate::ui::dialog::template::NONE.to_string()];
    names.extend(
        crate::viewer::fileinfo::builtin()
            .iter()
            .map(|t| t.name.clone()),
    );
    let picker = crate::ui::dialog::template::TemplateDialog::new(names, current.as_deref())
        .with_quick_search(
            app.config.panel.quick_search,
            app.config.panel.quick_search_case,
        );
    app.push_dialog(Box::new(picker));
}

/// Act on one resolved action with the viewer focused.
///
/// `extend` is what [`viewer_extend`] read off the modifiers: `Extend::None`
/// for a bare key, and the linear or rectangular extension for a `Shift` or
/// `Ctrl+Shift` movement. Only the ten movements of [`VIEWER_MOTIONS`] carry
/// it, and they all go through [`crate::viewer::Viewer::move_cursor`], so an
/// extension cannot be combined with a movement in ten different places and
/// come out differently.
fn viewer_action(app: &mut App, action: Action, extend: Extend) -> Result<()> {
    use Action as A;

    // Answered before the viewer is borrowed: building the request needs the
    // application, and the dialog is pushed onto it rather than onto the page.
    if action == A::FileInfo {
        super::open_viewer_file_info(app);
        return Ok(());
    }

    // `F1` is answered before the viewer is borrowed, because it opens another
    // viewer over this one.
    if action == A::Help {
        if app.viewer().is_some_and(crate::viewer::Viewer::is_help) {
            return Ok(());
        }
        let body = crate::ui::help::viewer_page(&app.keymap, app.keyboard.enhanced);
        app.request_view(crate::app::ViewRequest::Text {
            title: "Viewer keys".to_string(),
            body,
            help: true,
        });
        return Ok(());
    }
    // Answered before the viewer is borrowed, for the same reason `F1` is: the
    // dialog is pushed onto the application, not onto the page.
    if action == A::ViewerTemplate {
        open_template_picker(app);
        return Ok(());
    }
    if action == A::Close || action == A::View {
        app.pop_viewer();
        return Ok(());
    }
    if action == A::GotoOffset {
        // "`Ctrl+G` jumps to an offset, accepting `0x`
        // notation", and the percentage seek, which had no key
        // until this prompt learned to spell it. The prompt is a dialog over
        // the viewer, so the viewer keeps its position while the question is
        // asked.
        app.push_dialog(Box::new(InputDialog::new(
            DialogId::GotoOffset,
            "Go to offset",
            "Offset (0x…), 50% or :line:",
            "",
        )));
        return Ok(());
    }

    let rows = app
        .focused_viewer()
        .map_or(1, crate::viewer::Viewer::view_rows);
    // the "mode is remembered per session". Recorded here and
    // applied to `App` once the viewer borrow is over - the next file opened
    // starts in whatever mode this one was left in.
    let mut remembered: Option<crate::config::ViewerMode> = None;
    let Some(viewer) = app.focused_viewer_mut() else {
        // The stack emptied under us. Restore focus rather than sitting in a
        // `Focus::Viewer` with nothing behind it - the same repair
        // `dialog_key` makes.
        app.set_focus(Focus::Panel(app.active_side));
        return Ok(());
    };

    // Find and selection are byte-range tools, and mode 3 has no byte range
    // to give them: a rendered line is assembled from text the file may hold
    // in a dozen places. Saying so and naming the mode that can search is the
    // rule the whole viewer follows - a key that appears to do nothing is the
    // thing being avoided.
    if viewer.mode() == crate::config::ViewerMode::Render
        && matches!(
            action,
            A::QuickFind | A::FindNext | A::FindPrev | A::SelectAll | A::SelectBlock
        )
    {
        app.message = Some(
            "not available in mode 3 - it renders the document, not the file's bytes. Press 1 to search the text".to_string(),
        );
        return Ok(());
    }

    let outcome = match action {
        // `F7` / `/` / `Ctrl+F` open the bar, `n` / `Shift+N`
        // step. Answered before the borrow below because they report.
        A::QuickFind => {
            viewer.open_find();
            return Ok(());
        }
        A::FindNext | A::FindPrev => {
            // whatever searched last is the session's
            // pattern. Recorded here rather than in the find bar, because
            // this is the point at which a pattern has actually been used to
            // find something and not merely typed.
            let searched = viewer.find_query().clone();
            // "pressed with no pattern set, it opens the find
            // bar instead, so the key is never inert". Nothing has been
            // searched for in this session and there is nothing to step to.
            if searched.input.is_empty() {
                viewer.open_find();
                return Ok(());
            }
            let found = if action == A::FindPrev {
                viewer.find_prev()
            } else {
                viewer.find_next()
            };
            // This arm answers and returns, so the session pattern is stored
            // here rather than at the end of the function like `remembered`.
            // The borrow of `viewer` ends with `found`.
            if !searched.input.is_empty() {
                app.viewers.last_find = Some(searched);
            }
            report_find(app, found);
            return Ok(());
        }
        A::ModeText => {
            let outcome = viewer.set_mode(crate::config::ViewerMode::Text);
            remembered = Some(crate::config::ViewerMode::Text);
            outcome
        }
        A::ModeHex => {
            let outcome = viewer.set_mode(crate::config::ViewerMode::Hex);
            remembered = Some(crate::config::ViewerMode::Hex);
            outcome
        }
        // Mode 3 is the one mode that can decline to be entered, and the
        // session must not remember a mode the next file would also decline:
        // `remembered` is set only where it actually arrived.
        A::ModeRender => {
            let outcome = viewer.set_mode(crate::config::ViewerMode::Render);
            let note = viewer.render_note().map(str::to_string);
            if viewer.mode() == crate::config::ViewerMode::Render {
                remembered = Some(crate::config::ViewerMode::Render);
            }
            if let Some(said) = note {
                app.message = Some(said);
                return outcome;
            }
            outcome
        }
        A::FoldToggle => {
            let said = viewer.toggle_fold();
            app.message = Some(said);
            return Ok(());
        }
        A::FoldAll | A::UnfoldAll => {
            if viewer.mode() != crate::config::ViewerMode::Render {
                app.message = Some("folding is a mode 3 thing; press 3".to_string());
                return Ok(());
            }
            let said = viewer.fold_all(action == A::FoldAll);
            app.message = Some(said);
            return Ok(());
        }
        // `F4` toggles. It is `edit` on a panel and mode-toggle
        // here, resolved by context exactly as the design says.
        A::Edit => {
            let outcome = viewer.toggle_mode();
            remembered = Some(viewer.mode());
            outcome
        }
        A::ToggleWrap => {
            viewer.toggle_wrap();
            Ok(())
        }
        // Hex only: in text mode there is no byte under the cursor to read,
        // only a character, and the panel would be four empty lines.
        A::Inspect => {
            viewer.toggle_inspect();
            Ok(())
        }
        // `g`, `d`, `e`. Each reports what it changed to, because a decimal
        // column whose byte order is not stated is a number you cannot trust.
        // `F2` reloads in the viewer. It renames on a panel; the file-manager
        // meaning does not follow in here, and the design resolves that by
        // context.
        A::ViewerReload => {
            match viewer.reload() {
                Ok(said) => app.message = Some(said),
                Err(err) => app.message = Some(format!("reload failed: {err}")),
            }
            return Ok(());
        }
        A::HexGroup => {
            let said = viewer.cycle_hex_group();
            app.message = Some(said);
            return Ok(());
        }
        A::HexFormat => {
            let said = viewer.cycle_hex_format();
            app.message = Some(said);
            return Ok(());
        }
        A::HexSign => {
            let said = viewer.toggle_hex_sign();
            app.message = Some(said);
            return Ok(());
        }
        A::HexEndian => {
            let said = viewer.flip_hex_endian();
            app.message = Some(said);
            return Ok(());
        }
        A::CycleEncoding => viewer.cycle_encoding(),
        // `Ctrl` with a movement scrolls the **view**, and the
        // cursor and the selection stay exactly where they are.
        // `Viewer::scroll` already means that with a cursor enabled - it moves
        // `top` and nothing else - and `scroll_view_horizontal` is the
        // sideways half, which `scroll_horizontal` is not: that one routes
        // through the cursor when there is one.
        A::ViewScrollUp => viewer.scroll(-1),
        A::ViewScrollDown => viewer.scroll(1),
        A::ViewScrollPageUp => viewer.page(false),
        A::ViewScrollPageDown => viewer.page(true),
        A::ViewScrollLeft => {
            viewer.scroll_view_horizontal(-1);
            Ok(())
        }
        A::ViewScrollRight => {
            viewer.scroll_view_horizontal(1);
            Ok(())
        }
        // "the arrow keys move it rather than scrolling the
        // page; the view follows when it reaches an edge". Every one of these
        // goes through the single entry point, so the ten motions cannot be
        // combined with an extension in ten different places and come out
        // differently - and with `viewer.cursor = false` it is v0.4's page
        // scroll to the byte.
        A::CursorUp
        | A::CursorDown
        | A::CursorPageUp
        | A::CursorPageDown
        | A::CursorTop
        | A::CursorBottom
        | A::LineStart
        | A::LineEnd
        | A::CaretLeft
        | A::CaretRight => match viewer_motion(action) {
            Some(motion) => viewer.move_cursor(motion, extend),
            // Unreachable while this arm and `VIEWER_MOTIONS` name the same ten
            // actions, and a repair rather than a panic if they ever drift
            // apart (the "never a panic").
            None => Ok(()),
        },
        // "`Ctrl+A` selects the whole file", which costs nothing - `0..len`
        // without reading a byte. With no cursor there is nothing to select
        // from, and saying so beats doing nothing (the design item 4.
        A::SelectAll => {
            if viewer.cursor_enabled() {
                viewer.select_all();
            } else {
                app.message = Some(crate::viewer::copy::NO_CURSOR.to_string());
            }
            return Ok(());
        }
        // `Tab` switches the hex side and *nothing else
        // changes*. In text mode there are no sides, and a key that silently
        // did nothing would look broken.
        A::HexSide => {
            if viewer.mode() == crate::config::ViewerMode::Hex {
                viewer.switch_hex_side();
            } else {
                app.message = Some("hex side: hex mode only - 2 or F4 switches to it".to_string());
            }
            return Ok(());
        }
        // the `Ctrl+C` and `Ctrl+Shift+C`. Both **queue**: copying
        // reads the file and `dispatch` may not, so
        // `main::service_viewer_copy` performs it before the next layout. Every
        // refusal - no cursor, nothing selected, above `viewer.copy_max`, no
        // reading for this length - is decided there, where the numbers are.
        //
        A::ClipboardCopy => {
            viewer.request_copy(crate::viewer::copy::CopyRequest::Selection);
            return Ok(());
        }
        A::CopyInterpretation => {
            viewer.request_copy(crate::viewer::copy::CopyRequest::Interpretation);
            return Ok(());
        }
        // `Alt+B`, the documented alternate binding for rectangular extension:
        // it flips the
        // live selection's kind, anchor and head unmoved, so a selection
        // already made can be re-read as a column block.
        A::SelectBlock => {
            let said = if viewer.cursor_enabled() {
                match viewer.toggle_selection_kind() {
                    Some(crate::viewer::select::SelectKind::Rectangular) => {
                        "selection is a column block - Shift with the arrows widens it".to_string()
                    }
                    Some(crate::viewer::select::SelectKind::Linear) => {
                        "selection is linear again".to_string()
                    }
                    None => crate::viewer::copy::NOTHING_SELECTED.to_string(),
                }
            } else {
                crate::viewer::copy::NO_CURSOR.to_string()
            };
            app.message = Some(said);
            return Ok(());
        }
        // Everything else is consumed, because the design says the viewer
        // consumes all input. Saying *why* is the "never a panic and
        // never silence" - and the why here is not "nothing bound to do yet",
        // which belongs to an action whose milestone has not landed. `F10`,
        // `F5`, `Ctrl+O` and `Tab` are all shipped and all one keystroke away;
        // telling the user they do not exist yet would be the wrong answer to
        // the right question.
        other => {
            let _ = rows;
            app.message = Some(if other.implemented() {
                format!(
                    "{}: not available in the viewer - Esc closes it",
                    other.description()
                )
            } else {
                other.not_implemented_message()
            });
            return Ok(());
        }
    };
    if let Some(mode) = remembered {
        app.viewers.mode = Some(mode);
    }
    if let Err(err) = outcome {
        app.message = Some(err.to_string());
    }
    Ok(())
}
