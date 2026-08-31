//! Tests for the synchronise dialog: it lists every action, lets one row and
//! the whole direction change, and hands the plan back only on accept.

use super::*;
use crate::config::{ColorDepth, Theme};
use crate::input::KeyPress;
use crate::ops::sync::{PairState, SyncItem, SyncMode, SyncPlan};
use crate::vfs::VfsPath;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;

fn key(code: KeyCode) -> DialogKey {
    DialogKey::raw(KeyPress::plain(code))
}

fn item(rel: &str, state: PairState, action: SyncAction) -> SyncItem {
    SyncItem {
        rel: rel.to_string(),
        is_dir: false,
        state,
        action,
        left: VfsPath::local(format!("/left/{rel}")),
        right: VfsPath::local(format!("/right/{rel}")),
        left_size: Some(1_024),
        right_size: Some(2_048),
    }
}

fn dialog() -> SynchronizeDialog {
    let plan = SyncPlan::from_parts(
        vec![
            item("only-left.txt", PairState::LeftOnly, SyncAction::CopyRight),
            item("only-right.txt", PairState::RightOnly, SyncAction::CopyLeft),
            item("newer.txt", PairState::LeftNewer, SyncAction::CopyRight),
            item("clash.txt", PairState::Conflict, SyncAction::Skip),
        ],
        SyncMode::Both,
    );
    SynchronizeDialog::new(&VfsPath::local("/left"), &VfsPath::local("/right"), plan)
}

fn render(d: &SynchronizeDialog, w: u16, h: u16, ascii: bool) -> Buffer {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let st = DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, ascii);
    terminal
        .draw(|f| {
            crate::dialog::draw(f, d, f.area(), &st);
        })
        .expect("draw");
    terminal.backend().buffer().clone()
}

fn render_inner(d: &impl Dialog, w: u16, h: u16, ascii: bool) -> Buffer {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let st = DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, ascii);
    terminal.draw(|f| d.render(f, f.area(), &st)).expect("draw");
    terminal.backend().buffer().clone()
}

fn dump(buf: &Buffer) -> String {
    let area = buf.area();
    let mut out = String::new();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            out.push_str(buf.cell((x, y)).map_or(" ", |c| c.symbol()));
        }
        out.push('\n');
    }
    out
}

#[test]
fn it_lists_every_action_with_a_direction() {
    let out = dump(&render(&dialog(), 90, 20, false));
    for needle in [
        "copy >",
        "< copy",
        "skip",
        "only-left.txt",
        "only-right.txt",
    ] {
        assert!(out.contains(needle), "{needle} missing:\n{out}");
    }
}

#[test]
fn space_cycles_the_current_rows_choice() {
    let mut d = dialog();
    // The first row is a left-only file, defaulting to copy-right.
    assert_eq!(d.plan().items()[0].action, SyncAction::CopyRight);
    d.handle_key(&key(KeyCode::Char(' ')));
    // Left-only cycles copy-right -> delete-left -> skip.
    assert_eq!(d.plan().items()[0].action, SyncAction::DeleteLeft);
}

#[test]
fn tab_changes_the_direction_and_redefaults_every_row() {
    let mut d = dialog();
    d.handle_key(&key(KeyCode::Tab));
    assert_eq!(d.plan().mode(), SyncMode::ToRight);
    // Under a mirror onto the right, the right-only file is now a deletion.
    let right_only = d
        .plan()
        .items()
        .iter()
        .find(|i| i.rel == "only-right.txt")
        .expect("the right-only row");
    assert_eq!(right_only.action, SyncAction::DeleteRight);
    let out = dump(&render(&d, 90, 20, false));
    assert!(
        out.contains("Mirror"),
        "the header names the direction:\n{out}"
    );
}

#[test]
fn enter_hands_the_plan_back() {
    let mut d = dialog();
    match d.handle_key(&key(KeyCode::Enter)) {
        DialogOutcome::Accept(DialogResult::Synchronize(plan)) => {
            let c = plan.counts();
            // Two copy-rights (only-left, newer), one copy-left, one skip.
            assert_eq!(c.copy_right, 2);
            assert_eq!(c.copy_left, 1);
            assert_eq!(c.skip, 1);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn esc_cancels_and_changes_nothing() {
    let mut d = dialog();
    assert!(matches!(
        d.handle_key(&key(KeyCode::Esc)),
        DialogOutcome::Cancel
    ));
}

#[test]
fn the_cursor_clamps_to_the_list() {
    let mut d = dialog();
    for _ in 0..20 {
        d.handle_key(&key(KeyCode::Down));
    }
    // Four rows: the cursor lands on the last one and cycling it there proves
    // the cursor got there. A conflict cycles skip -> copy-right.
    d.handle_key(&key(KeyCode::Char(' ')));
    assert_eq!(d.plan().items()[3].action, SyncAction::CopyRight);
}

#[test]
fn it_renders_at_every_size_the_spec_names() {
    for (w, h) in [(200u16, 50u16), (80, 24), (60, 15)] {
        for ascii in [false, true] {
            let out = dump(&render(&dialog(), w, h, ascii));
            assert!(out.contains("apply"), "{w}x{h} ascii={ascii}:\n{out}");
        }
    }
}

#[test]
fn every_glyph_it_draws_has_an_ascii_form() {
    for (w, h) in [(200u16, 50u16), (80, 24), (60, 15), (20, 3)] {
        let inner = dump(&render_inner(&dialog(), w, h, true));
        assert!(inner.is_ascii(), "{w}x{h}:\n{inner}");
    }
}

#[test]
fn an_empty_plan_says_the_trees_match() {
    let plan = SyncPlan::from_parts(Vec::new(), SyncMode::Both);
    let d = SynchronizeDialog::new(&VfsPath::local("/l"), &VfsPath::local("/r"), plan);
    let out = dump(&render(&d, 70, 12, false));
    assert!(out.contains("already the same"), "{out}");
}

#[test]
fn it_declares_no_mnemonics() {
    // A list plus two whole-list keys; there is no focusable control, so the
    // empty answer is the decision, as it is for the queue view.
    assert!(dialog().mnemonic_letters().is_empty());
}
