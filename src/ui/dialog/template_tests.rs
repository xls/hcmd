//! The template picker: cursor, quick search, and the downcast the caller
//! needs.

use super::*;
use crate::dialog::{Dialog, DialogKey, DialogOutcome, DialogResult};
use crate::input::binding::KeyPress;

fn names() -> Vec<String> {
    ["ELF64", "GIF", "GPT header", "PNG", "ZIP local header"]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

fn dialog() -> TemplateDialog {
    TemplateDialog::new(names(), None)
}

fn press(dialog: &mut TemplateDialog, code: KeyCode) -> DialogOutcome {
    dialog.handle_key(&DialogKey::raw(KeyPress::plain(code)))
}

#[test]
fn it_opens_on_the_applied_template() {
    let dialog = TemplateDialog::new(names(), Some("PNG"));
    assert_eq!(dialog.cursor(), 3);
    assert_eq!(dialog.selected(), Some("PNG"));
}

#[test]
fn a_name_that_is_not_there_opens_at_the_top() {
    let dialog = TemplateDialog::new(names(), Some("nothing"));
    assert_eq!(dialog.cursor(), 0);
}

#[test]
fn the_cursor_walks_and_stops_at_both_ends() {
    let mut dialog = dialog();
    press(&mut dialog, KeyCode::Up);
    assert_eq!(dialog.cursor(), 0);
    press(&mut dialog, KeyCode::End);
    assert_eq!(dialog.cursor(), 4);
    press(&mut dialog, KeyCode::Down);
    assert_eq!(dialog.cursor(), 4);
    press(&mut dialog, KeyCode::Home);
    assert_eq!(dialog.cursor(), 0);
}

#[test]
fn quick_search_moves_to_the_first_match_and_refuses_a_dead_end() {
    let mut dialog = dialog();
    assert!(matches!(
        press(&mut dialog, KeyCode::Char('z')),
        DialogOutcome::Consumed
    ));
    assert_eq!(dialog.selected(), Some("ZIP local header"));
    // 'q' after 'z' matches nothing, so it is not typed and the cursor stays.
    assert!(matches!(
        press(&mut dialog, KeyCode::Char('q')),
        DialogOutcome::Ignored
    ));
    assert_eq!(dialog.quick_buffer(), "z");
    assert_eq!(dialog.selected(), Some("ZIP local header"));
}

#[test]
fn backspace_re_runs_the_shorter_search() {
    let mut dialog = dialog();
    press(&mut dialog, KeyCode::Char('g'));
    press(&mut dialog, KeyCode::Char('p'));
    assert_eq!(dialog.selected(), Some("GPT header"));
    press(&mut dialog, KeyCode::Backspace);
    assert_eq!(dialog.quick_buffer(), "g");
    assert_eq!(dialog.selected(), Some("GIF"));
}

#[test]
fn moving_the_cursor_ends_the_search() {
    let mut dialog = dialog();
    press(&mut dialog, KeyCode::Char('g'));
    assert_eq!(dialog.quick_buffer(), "g");
    press(&mut dialog, KeyCode::Down);
    assert!(dialog.quick_buffer().is_empty());
}

#[test]
fn enter_answers_with_the_name_and_esc_cancels() {
    let mut dialog = TemplateDialog::new(names(), Some("GIF"));
    let accept = dialog.handle_key(&DialogKey::raw(KeyPress::plain(KeyCode::Enter)));
    match accept {
        DialogOutcome::Accept(DialogResult::Text(name)) => assert_eq!(name, "GIF"),
        other => panic!("expected the name back, got {other:?}"),
    }
    assert!(matches!(
        dialog.handle_key(&DialogKey::raw(KeyPress::plain(KeyCode::Esc))),
        DialogOutcome::Cancel
    ));
}

#[test]
fn an_empty_list_answers_nothing_rather_than_panicking() {
    let mut dialog = TemplateDialog::new(Vec::new(), None);
    assert_eq!(dialog.selected(), None);
    press(&mut dialog, KeyCode::Down);
    assert!(matches!(
        dialog.handle_key(&DialogKey::raw(KeyPress::plain(KeyCode::Enter))),
        DialogOutcome::Cancel
    ));
}

/// The bug the theme picker's own comment warns about: a missing `as_any`
/// override is not a compile error, it just makes the downcast fail silently.
#[test]
fn it_can_be_downcast_by_the_caller() {
    let dialog = dialog();
    let boxed: Box<dyn Dialog> = Box::new(dialog);
    let found = boxed
        .as_any()
        .and_then(|any| any.downcast_ref::<TemplateDialog>());
    assert_eq!(found.and_then(TemplateDialog::selected), Some("ELF64"));
}

#[test]
fn it_reports_its_own_id_and_a_capped_width() {
    let dialog = dialog();
    assert_eq!(dialog.id(), DialogId::Template);
    assert_eq!(dialog.title(), "Template");
    let (width, height) = dialog.size_hint();
    assert!(width <= MAX_WIDTH, "{width} is wider than the cap");
    assert_eq!(height, LIST_ROWS + 2);
}

#[test]
fn the_window_follows_the_cursor_by_pages() {
    let long: Vec<String> = (0..40).map(|i| format!("t{i:02}")).collect();
    let mut dialog = TemplateDialog::new(long, None);
    assert_eq!(dialog.window(10), 0..10);
    press(&mut dialog, KeyCode::PageDown);
    assert_eq!(dialog.cursor(), 14);
    assert_eq!(dialog.window(10), 10..20);
}
