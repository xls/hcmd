//! The file-information answer: the filesystem half, the contents half, and
//! the cases where there is no contents half at all.

use super::*;
use crate::viewer::template::Template;

fn facts<'a>(name: &'a str, size: u64, attrs: &'a str) -> FileFacts<'a> {
    FileFacts::new(name, size, attrs)
}

/// One fact line's value, or a failure naming the missing label.
fn fact(info: &FileInfo, label: &str) -> String {
    info.facts
        .iter()
        .find(|line| line.label == label)
        .map(|line| line.value.clone())
        .unwrap_or_else(|| panic!("no fact called {label:?} in {:#?}", info.facts))
}

fn png() -> Vec<u8> {
    let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    png.extend_from_slice(&13_u32.to_be_bytes());
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&800_u32.to_be_bytes());
    png.extend_from_slice(&600_u32.to_be_bytes());
    png.extend_from_slice(&[8, 2, 0, 0, 0]);
    png.extend_from_slice(&0_u32.to_be_bytes());
    png
}

#[test]
fn the_filesystem_half_is_there_for_every_file() {
    let bytes = png();
    let info = describe(&facts("shot.png", 24_576, "-rw-r--r--"), &bytes);
    assert_eq!(info.name, "shot.png");
    assert_eq!(fact(&info, "Size"), "24,576 bytes (24 KB)");
    assert_eq!(fact(&info, "Attributes"), "-rw-r--r--");
    assert!(info.note.is_none());
}

#[test]
fn a_date_is_shown_only_when_the_caller_has_one() {
    let bytes = png();
    let plain = describe(&facts("a.png", 10, "-rw-r--r--"), &bytes);
    assert!(plain.facts.iter().all(|line| line.label != "Modified"));
    let dated = describe(
        &facts("a.png", 10, "-rw-r--r--").modified("2024-03-01 09:15"),
        &bytes,
    );
    assert_eq!(fact(&dated, "Modified"), "2024-03-01 09:15");
}

#[test]
fn a_small_size_is_exact_and_a_large_one_is_both() {
    let bytes = png();
    let small = describe(&facts("a.png", 900, "-rw-r--r--"), &bytes);
    assert_eq!(fact(&small, "Size"), "900 bytes");
    let large = describe(&facts("a.png", 5_242_880, "-rw-r--r--"), &bytes);
    assert_eq!(fact(&large, "Size"), "5,242,880 bytes (5.0 MB)");
}

#[test]
fn the_contents_half_names_the_format_and_summarises_it() {
    let bytes = png();
    let info = describe(&facts("shot.png", 24_576, "-rw-r--r--"), &bytes);
    assert_eq!(info.format.as_deref(), Some("PNG image"));
    let rendered: Vec<(&str, &str)> = info
        .lines
        .iter()
        .map(|line| (line.label.as_str(), line.value.as_str()))
        .collect();
    assert!(
        rendered.contains(&("Dimensions", "800 x 600 px")),
        "{rendered:#?}"
    );
    assert!(rendered.contains(&("Colour", "RGB")), "{rendered:#?}");
}

#[test]
fn a_directory_says_it_is_one_rather_than_that_it_was_not_recognised() {
    let info = describe(&facts("src", 4096, "drwxr-xr-x").directory(true), &[]);
    assert_eq!(info.format, None);
    assert!(info.lines.is_empty());
    assert_eq!(info.note.as_deref(), Some("A directory. Enter opens it."));
    // The filesystem half is still there, which is the whole point.
    assert_eq!(fact(&info, "Attributes"), "drwxr-xr-x");
}

#[test]
fn an_empty_file_says_so() {
    let info = describe(&facts("empty", 0, "-rw-r--r--"), &[]);
    assert_eq!(info.note.as_deref(), Some("The file is empty."));
    assert_eq!(fact(&info, "Size"), "0 bytes");
}

#[test]
fn a_file_no_template_claims_still_opens_with_something_to_read() {
    let info = describe(
        &facts("notes.txt", 41, "-rw-r--r--"),
        b"just some words in a plain text file.\n",
    );
    assert_eq!(info.format, None);
    assert!(info.lines.is_empty());
    assert_eq!(
        info.note.as_deref(),
        Some("The contents match no template this program has.")
    );
    assert_eq!(fact(&info, "Size"), "41 bytes");
    assert_eq!(info.name, "notes.txt");
}

#[test]
fn a_format_with_no_summary_of_its_own_says_that_rather_than_nothing() {
    // The DER template is recognised and deliberately carries no summary.
    let info = describe(
        &facts("cert.der", 1200, "-rw-r--r--"),
        &[0x30, 0x82, 0x04, 0x00],
    );
    assert_eq!(info.format.as_deref(), Some("DER sequence"));
    assert!(info.lines.is_empty());
    assert!(
        info.note
            .as_deref()
            .is_some_and(|note| note.contains("no summary of its own")),
        "{:?}",
        info.note
    );
}

#[test]
fn an_attribute_string_the_caller_does_not_have_leaves_the_line_out() {
    let bytes = png();
    let info = describe(&facts("a.png", 10, ""), &bytes);
    assert!(info.facts.iter().all(|line| line.label != "Attributes"));
    assert_eq!(fact(&info, "Size"), "10 bytes");
}

#[test]
fn a_short_read_summarises_what_the_file_actually_holds() {
    let full = png();
    let short = full.get(..24).expect("24 of 33 bytes");
    let info = describe(&facts("cut.png", 24, "-rw-r--r--"), short);
    assert_eq!(info.format.as_deref(), Some("PNG image"));
    // Width and height are both there at 24 bytes; the colour type is not, so
    // its line is absent rather than wrong.
    let labels: Vec<&str> = info.lines.iter().map(|l| l.label.as_str()).collect();
    assert!(labels.contains(&"Dimensions"), "{labels:?}");
    assert!(!labels.contains(&"Colour"), "{labels:?}");
}

#[test]
fn describe_with_uses_the_set_it_is_given() {
    let template = Template::parse(
        r#"
name  = "Toy"
magic = { offset = 0, bytes = "5A5A" }

[[field]]
name = "count"
type = "u8"
offset = 2

[summary]
title = "A toy format"

[[summary.line]]
label = "Count"
field = "count"
"#,
    )
    .expect("parses");
    let set = vec![template];
    let info = describe_with(&set, &facts("t.bin", 3, "-rw-r--r--"), &[0x5A, 0x5A, 7]);
    assert_eq!(info.format.as_deref(), Some("A toy format"));
    assert_eq!(info.lines.first().map(|l| l.value.as_str()), Some("7"));
    // The same bytes against an empty set: recognised by nothing.
    let none = describe_with(&[], &facts("t.bin", 3, "-rw-r--r--"), &[0x5A, 0x5A, 7]);
    assert!(none.format.is_none());
    assert!(none.note.is_some());
}

#[test]
fn every_builtin_template_parses() {
    // `template_data::BUILTIN` filters out anything that will not parse, so a
    // count short of the file count would be a template silently missing from
    // the program.
    assert_eq!(
        builtin().len(),
        BUILTIN.len(),
        "a built-in template did not parse"
    );
    assert!(builtin().len() > 100);
}

/// The head a caller must read has to reach the furthest byte any template
/// needs, or that template can never match.
#[test]
fn builtin_templates_all_fit_in_the_head() {
    let mut furthest = 0_usize;
    let mut deepest = "";
    for template in builtin() {
        let end = template.offset.saturating_add(template.span);
        let magic_end = template
            .magic
            .as_ref()
            .map_or(0, |m| m.offset.saturating_add(m.bytes.len()));
        let needs = end.max(magic_end);
        if needs > furthest {
            furthest = needs;
            deepest = template.name.as_str();
        }
    }
    assert!(
        furthest <= HEAD_BYTES,
        "{deepest} needs {furthest} bytes and HEAD_BYTES is {HEAD_BYTES}"
    );
    // And not absurdly larger than it needs to be, or every caller reads a
    // megabyte to look at a text file.
    assert!(
        HEAD_BYTES < furthest.saturating_mul(4),
        "HEAD_BYTES is far larger than the {furthest} bytes anything needs"
    );
}

#[test]
fn a_template_deep_in_the_file_is_found_when_the_head_reaches_it() {
    let mut image = vec![0_u8; 32768 + 2048];
    image[32768] = 1;
    image[32769..32774].copy_from_slice(b"CD001");
    image[32768 + 40..32768 + 48].copy_from_slice(b"DEBIAN12");
    let info = describe(&facts("debian.iso", 700_000_000, "-rw-r--r--"), &image);
    assert_eq!(info.format.as_deref(), Some("ISO 9660 disc image"));
    assert!(
        info.lines
            .iter()
            .any(|l| l.label == "Volume" && l.value == "DEBIAN12"),
        "{:#?}",
        info.lines
    );
    // The same file read only 4 KB deep: nothing to recognise it by, and the
    // answer says so instead of guessing.
    let shallow = image.get(..4096).expect("4 KB");
    let info = describe(&facts("debian.iso", 700_000_000, "-rw-r--r--"), shallow);
    assert!(info.format.is_none());
    assert!(info.note.is_some());
}
