use super::*;
use crate::config::Config;

/// The shape the generated file actually has: every line commented out.
fn commented_example() -> String {
    crate::config::EXAMPLE_CONFIG
        .lines()
        .map(|l| {
            if l.is_empty() {
                "#".to_string()
            } else {
                format!("# {l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

#[test]
fn a_live_setting_has_only_its_value_replaced() {
    let text = "[ui]\ntheme          = \"blue\"      # a shipped name\nmouse = false\n";
    let out = set_theme(text, "dracula");
    assert!(out.starts_with("[ui]\n"), "{out}");
    assert!(out.ends_with("mouse = false\n"), "{out}");
    assert!(out.contains("theme          = \"dracula\""), "{out}");
    assert!(out.contains("# a shipped name"), "{out}");
}

/// The note keeps its column, which is what "aligned" means in this file.
/// The gap either side of it changes with the length of the name; the column
/// the reader's eye follows does not.
#[test]
fn a_longer_or_shorter_name_leaves_the_note_in_its_column() {
    let text = "theme          = \"blue\"      # a shipped name";
    let column = text.find('#').expect("the note is there");
    for name in ["nord", "dracula", "blue"] {
        let out = set_theme(&format!("[ui]\n{text}\n"), name);
        let line = out.lines().nth(1).expect("the setting line");
        assert_eq!(
            line.find('#'),
            Some(column),
            "the note moved for {name}: {line:?}"
        );
    }
    // A name too long for the column pushes the note right by one space, and
    // no more than one.
    let out = set_theme(&format!("[ui]\n{text}\n"), "catppuccin-latte");
    let line = out.lines().nth(1).expect("the setting line");
    let value_ends = line.find("\"catppuccin-latte\"").map(|at| at + 18);
    assert_eq!(
        line.find('#'),
        value_ends.map(|e| e + 1),
        "a long name should push the note by exactly one space: {line:?}"
    );
}

#[test]
fn the_alignment_and_the_trailing_note_both_survive() {
    let text = "[ui]\ntheme          = \"blue\"      # themes/<name>.toml\n";
    let out = set_theme(text, "nord");
    assert!(out.contains("theme          = \"nord\""), "{out}");
    assert!(out.contains("# themes/<name>.toml"), "{out}");
}

#[test]
fn a_commented_theme_is_documentation_and_is_not_edited() {
    let text = "[ui]\n# theme = \"blue\"   # the default\n";
    let out = set_theme(text, "nord");
    assert!(
        out.contains("# theme = \"blue\"   # the default"),
        "the comment was rewritten:\n{out}"
    );
    assert!(
        out.contains("\ntheme = \"nord\"\n"),
        "no live setting added:\n{out}"
    );
}

#[test]
fn the_new_setting_goes_directly_under_the_ui_header() {
    let text = "[panel]\nhidden = false\n\n[ui]\n# theme = \"blue\"\nmouse = false\n";
    let out = set_theme(text, "nord");
    let lines: Vec<&str> = out.lines().collect();
    let at = lines.iter().position(|l| *l == "[ui]").expect("[ui] kept");
    assert_eq!(
        lines.get(at.saturating_add(1)).copied(),
        Some("theme = \"nord\""),
        "{out}"
    );
}

#[test]
fn a_file_with_no_ui_section_gains_one() {
    let text = "[panel]\nhidden = false\n";
    let out = set_theme(text, "nord");
    assert!(out.contains("[panel]\nhidden = false\n"), "{out}");
    assert!(out.contains("[ui]\ntheme = \"nord\""), "{out}");
}

#[test]
fn an_empty_file_becomes_the_one_setting_that_was_chosen() {
    assert_eq!(set_theme("", "nord"), "[ui]\ntheme = \"nord\"");
}

#[test]
fn a_theme_key_in_another_section_is_not_the_one() {
    let text = "[viewer]\ntheme = \"mono\"\n\n[ui]\nmouse = false\n";
    let out = set_theme(text, "nord");
    assert!(
        out.contains("[viewer]\ntheme = \"mono\""),
        "wrong section edited:\n{out}"
    );
    assert!(out.contains("[ui]\ntheme = \"nord\""), "{out}");
}

#[test]
fn the_dotted_form_is_the_same_setting() {
    let text = "ui.theme = \"blue\"\n\n[panel]\nhidden = false\n";
    let out = set_theme(text, "nord");
    assert!(out.contains("ui.theme = \"nord\""), "{out}");
    assert!(
        !out.contains("[ui]"),
        "a second place to set it was created:\n{out}"
    );
}

#[test]
fn the_trailing_newline_is_neither_added_nor_dropped() {
    assert!(set_theme("[ui]\nmouse = false\n", "nord").ends_with('\n'));
    assert!(!set_theme("[ui]\nmouse = false", "nord").ends_with('\n'));
}

#[test]
fn writing_twice_leaves_one_setting_not_two() {
    let once = set_theme(&commented_example(), "nord");
    let twice = set_theme(&once, "dracula");
    assert_eq!(
        twice
            .lines()
            .filter(|l| is_live(l) && assigns(l, "theme"))
            .count(),
        1,
        "a second live theme was added:\n{twice}"
    );
    assert!(twice.contains("theme = \"dracula\""), "{twice}");
}

#[test]
fn the_generated_file_round_trips_through_the_real_parser() {
    let out = set_theme(&commented_example(), "kanagawa");
    let parsed: Config = toml::from_str(&out).expect("the edited file still parses");
    assert_eq!(parsed.ui.theme, "kanagawa");
}

#[test]
fn the_shipped_example_round_trips_through_the_real_parser() {
    let out = set_theme(crate::config::EXAMPLE_CONFIG, "synthwave");
    let parsed: Config = toml::from_str(&out).expect("the edited file still parses");
    assert_eq!(parsed.ui.theme, "synthwave");
}

#[test]
fn every_other_line_of_the_shipped_example_is_untouched() {
    let before = crate::config::EXAMPLE_CONFIG;
    let after = set_theme(before, "material");
    let differing: Vec<(&str, &str)> = before
        .lines()
        .zip(after.lines())
        .filter(|(a, b)| a != b)
        .collect();
    assert_eq!(
        differing.len(),
        1,
        "more than the theme changed: {differing:?}"
    );
    assert!(
        differing
            .first()
            .is_some_and(|(_, b)| b.contains("material")),
        "{differing:?}"
    );
}
