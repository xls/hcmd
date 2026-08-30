//! The file information dialog: what it lays out, and how it scrolls.

use super::*;
use crate::dialog::DialogKey;
use crate::input::binding::KeyPress;
use crate::viewer::fileinfo::{FileFacts, describe};

fn png() -> Vec<u8> {
    let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    png.extend_from_slice(&13_u32.to_be_bytes());
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&1920_u32.to_be_bytes());
    png.extend_from_slice(&1080_u32.to_be_bytes());
    png.extend_from_slice(&[8, 6, 0, 0, 0]);
    png.extend_from_slice(&0_u32.to_be_bytes());
    png
}

fn dialog_for(name: &str, size: u64, attrs: &str, head: &[u8]) -> FileInfoDialog {
    let facts = FileFacts::new(name, size, attrs).modified("2024-03-01 09:15");
    FileInfoDialog::new(&describe(&facts, head))
}

fn press(dialog: &mut FileInfoDialog, code: KeyCode) -> DialogOutcome {
    dialog.handle_key(&DialogKey::raw(KeyPress::plain(code)))
}

#[test]
fn it_shows_the_filesystem_half_then_the_format_half() {
    let bytes = png();
    let dialog = dialog_for("screenshot.png", 24_576, "-rw-r--r--", &bytes);
    let rows = dialog.text_rows();
    let joined = rows.join("\n");
    assert!(joined.contains("24,576 bytes (24 KB)"), "{joined}");
    assert!(joined.contains("-rw-r--r--"), "{joined}");
    assert!(joined.contains("2024-03-01 09:15"), "{joined}");
    assert!(joined.contains("PNG image"), "{joined}");
    assert!(joined.contains("1920 x 1080 px"), "{joined}");
    assert!(joined.contains("RGBA"), "{joined}");
    // The name is the title, not a row.
    assert_eq!(dialog.title(), "screenshot.png");
    // The two halves are separated by a blank row.
    assert!(rows.iter().any(String::is_empty), "{rows:#?}");
}

#[test]
fn the_labels_line_up_in_one_column() {
    let bytes = png();
    let dialog = dialog_for("a.png", 1000, "-rw-r--r--", &bytes);
    // The value starts at the first non-space after the padding, and the
    // padding is the first run of two or more spaces in the row.
    let starts: Vec<usize> = dialog
        .text_rows()
        .iter()
        .filter_map(|row| {
            let gap = row.find("  ")?;
            let rest = row.get(gap..)?;
            let spaces = rest.len() - rest.trim_start().len();
            Some(gap + spaces)
        })
        .collect();
    assert!(starts.len() >= 4, "too few pair rows: {starts:?}");
    let first = starts.first().copied().unwrap_or(0);
    assert!(
        starts.iter().all(|s| *s == first),
        "the value column moves: {starts:?}"
    );
}

#[test]
fn a_file_nothing_recognises_still_has_rows_and_a_reason() {
    let dialog = dialog_for(
        "notes.txt",
        41,
        "-rw-r--r--",
        b"plain text, and nothing else\n",
    );
    let joined = dialog.text_rows().join("\n");
    assert!(joined.contains("41 bytes"), "{joined}");
    assert!(joined.contains("-rw-r--r--"), "{joined}");
    assert!(
        joined.contains("match no template"),
        "there is no plain answer: {joined}"
    );
    assert!(dialog.row_count() >= 4);
}

#[test]
fn a_directory_says_so() {
    let facts = FileFacts::new("src", 4096, "drwxr-xr-x").directory(true);
    let dialog = FileInfoDialog::new(&describe(&facts, &[]));
    let joined = dialog.text_rows().join("\n");
    assert!(joined.contains("drwxr-xr-x"), "{joined}");
    assert!(joined.contains("A directory"), "{joined}");
}

#[test]
fn both_closing_keys_close_it_and_neither_answers_anything() {
    let bytes = png();
    let mut dialog = dialog_for("a.png", 10, "-rw-r--r--", &bytes);
    assert!(matches!(
        dialog.handle_key(&DialogKey::raw(KeyPress::plain(KeyCode::Esc))),
        DialogOutcome::Cancel
    ));
    assert!(matches!(
        dialog.handle_key(&DialogKey::raw(KeyPress::plain(KeyCode::Enter))),
        DialogOutcome::Cancel
    ));
    assert!(matches!(
        press(&mut dialog, KeyCode::Tab),
        DialogOutcome::Ignored
    ));
}

#[test]
fn it_scrolls_only_as_far_as_there_is_content() {
    let bytes = png();
    let mut dialog = dialog_for("a.png", 10, "-rw-r--r--", &bytes);
    dialog.layout(Rect {
        x: 0,
        y: 0,
        width: 40,
        height: 4,
    });
    assert_eq!(dialog.scroll(), 0);
    press(&mut dialog, KeyCode::Up);
    assert_eq!(dialog.scroll(), 0, "it scrolled above the first row");
    press(&mut dialog, KeyCode::End);
    assert_eq!(dialog.scroll(), dialog.row_count().saturating_sub(4));
    press(&mut dialog, KeyCode::Down);
    assert_eq!(dialog.scroll(), dialog.row_count().saturating_sub(4));
    press(&mut dialog, KeyCode::Home);
    assert_eq!(dialog.scroll(), 0);
    press(&mut dialog, KeyCode::PageDown);
    assert!(dialog.scroll() > 0);
}

#[test]
fn a_box_taller_than_the_content_does_not_scroll_at_all() {
    let bytes = png();
    let mut dialog = dialog_for("a.png", 10, "-rw-r--r--", &bytes);
    dialog.layout(Rect {
        x: 0,
        y: 0,
        width: 40,
        height: 40,
    });
    press(&mut dialog, KeyCode::PageDown);
    assert_eq!(dialog.scroll(), 0);
}

#[test]
fn it_reports_its_own_id_and_a_capped_size() {
    let bytes = png();
    let dialog = dialog_for(
        "a-very-long-file-name-that-goes-on.png",
        10,
        "-rw-r--r--",
        &bytes,
    );
    assert_eq!(dialog.id(), DialogId::FileSummary);
    let (width, height) = dialog.size_hint();
    assert!(width <= MAX_WIDTH, "{width} is wider than the cap");
    assert!(height <= MAX_ROWS + 2, "{height} is taller than the cap");
    // Wide enough for the title, which is the file name.
    assert!(width >= 40, "{width} would crop the name");
}

#[test]
fn it_can_be_downcast_by_the_caller() {
    let bytes = png();
    let dialog = dialog_for("a.png", 10, "-rw-r--r--", &bytes);
    let boxed: Box<dyn Dialog> = Box::new(dialog);
    let found = boxed
        .as_any()
        .and_then(|any| any.downcast_ref::<FileInfoDialog>());
    assert!(found.is_some_and(|d| d.row_count() > 0));
}

#[test]
fn it_renders_into_every_size_without_panicking() {
    let bytes = png();
    let dialog = dialog_for("a.png", 10, "-rw-r--r--", &bytes);
    let style = DialogStyle::new(
        &crate::config::Theme::blue(),
        crate::config::ColorDepth::TrueColor,
        false,
    );
    for (width, height) in [(60_u16, 20_u16), (40, 10), (20, 3), (1, 1), (0, 0)] {
        let backend = ratatui::backend::TestBackend::new(width.max(1), height.max(1));
        let mut terminal = ratatui::Terminal::new(backend).expect("a test terminal");
        terminal
            .draw(|f| {
                dialog.render(
                    f,
                    Rect {
                        x: 0,
                        y: 0,
                        width,
                        height,
                    },
                    &style,
                );
            })
            .expect("draws");
    }
}
