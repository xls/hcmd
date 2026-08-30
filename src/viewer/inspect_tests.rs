use super::*;

fn hex_row(offset: u64, bytes: &[u8]) -> Row {
    Row::Hex {
        offset,
        bytes: bytes.to_vec(),
        matches: Vec::new(),
        sel: None,
        cursor: None,
    }
}

fn value_of<'a>(readings: &'a [Reading], label: &str) -> Option<&'a str> {
    readings
        .iter()
        .find(|r| r.label == label)
        .map(|r| r.value.as_str())
}

#[test]
fn the_bytes_come_from_the_cursor_forward() {
    let rows = [hex_row(0, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9])];
    assert_eq!(bytes_at(&rows, 2), vec![2, 3, 4, 5, 6, 7, 8, 9]);
}

#[test]
fn a_run_that_crosses_a_row_is_stitched() {
    let rows = [hex_row(0, &[0, 1, 2, 3]), hex_row(4, &[4, 5, 6, 7, 8])];
    assert_eq!(bytes_at(&rows, 3), vec![3, 4, 5, 6, 7, 8]);
}

#[test]
fn the_end_of_the_file_returns_short_rather_than_padding() {
    let rows = [hex_row(0, &[1, 2, 3])];
    assert_eq!(bytes_at(&rows, 1), vec![2, 3]);
    assert_eq!(bytes_at(&rows, 3), Vec::<u8>::new());
}

#[test]
fn both_byte_orders_are_always_offered() {
    // 0x0102 one way round, 0x0201 the other.
    let r = readings(&[0x01, 0x02]);
    assert_eq!(value_of(&r, "u16 (LE)"), Some("513"));
    assert_eq!(value_of(&r, "u16 (BE)"), Some("258"));
}

#[test]
fn a_signed_reading_appears_only_where_it_differs() {
    let positive = readings(&[0x01, 0x02]);
    assert!(
        value_of(&positive, "i16 (LE)").is_none(),
        "513 twice over says nothing: {positive:?}"
    );
    let negative = readings(&[0xFF, 0xFF]);
    assert_eq!(value_of(&negative, "u16 (LE)"), Some("65535"));
    assert_eq!(value_of(&negative, "i16 (LE)"), Some("-1"));
}

#[test]
fn one_byte_is_read_as_a_byte_a_sign_a_glyph_and_bits() {
    let r = readings(&[0x41]);
    assert_eq!(value_of(&r, "u8"), Some("65"));
    assert!(
        value_of(&r, "i8").is_none(),
        "65 twice over says nothing: {r:?}"
    );
    assert_eq!(value_of(&r, "char"), Some("'A'"));
    assert_eq!(value_of(&r, "bin"), Some("01000001"));
}

#[test]
fn a_high_byte_is_negative_when_signed() {
    let r = readings(&[0xFF]);
    assert_eq!(value_of(&r, "u8"), Some("255"));
    assert_eq!(value_of(&r, "i8"), Some("-1"));
}

#[test]
fn floats_are_read_at_four_and_eight_bytes() {
    // 1.0f32 is 0x3F800000, little-endian on the wire below.
    let r = readings(&[0x00, 0x00, 0x80, 0x3F]);
    assert_eq!(value_of(&r, "f32 (LE)"), Some("1"));
    // 1.0f64 is 0x3FF0000000000000.
    let r = readings(&[0, 0, 0, 0, 0, 0, 0xF0, 0x3F]);
    assert_eq!(value_of(&r, "f64 (LE)"), Some("1"));
}

#[test]
fn a_width_the_file_does_not_have_is_not_offered() {
    let r = readings(&[0x01, 0x02, 0x03]);
    assert!(value_of(&r, "u16 (LE)").is_some());
    assert!(
        value_of(&r, "u32 (LE)").is_none(),
        "three bytes cannot be read as four: {r:?}"
    );
}

#[test]
fn a_plausible_timestamp_is_named_and_an_implausible_one_is_not() {
    // 2021-01-01 00:00:00 UTC is 1609459200 = 0x5FEE6600.
    let r = readings(&0x5FEE_6600_u32.to_le_bytes());
    assert_eq!(value_of(&r, "time (LE)"), Some("2021-01-01 00:00:00"));
    // A run of zeros is not a date anyone stored.
    let zeros = readings(&[0, 0, 0, 0]);
    assert!(
        value_of(&zeros, "time (LE)").is_none(),
        "1970 for four zero bytes is noise: {zeros:?}"
    );
}

#[test]
fn nothing_at_all_reads_as_nothing() {
    assert!(readings(&[]).is_empty());
}

#[test]
fn the_widest_label_is_what_the_panel_lays_out_against() {
    let r = readings(&[1, 2, 3, 4, 5, 6, 7, 8]);
    let want = r.iter().map(|x| x.label.chars().count()).max().unwrap_or(0);
    assert_eq!(label_width(&r), want);
    assert!(want > 0);
}

/// The two readers must not disagree about the same bytes.
#[test]
fn it_agrees_with_the_status_lines_reading() {
    use crate::config::{Endian, HexConfig};

    let bytes = [0x61, 0x62, 0x63, 0x64];
    let cfg = HexConfig::default();
    let line = crate::viewer::copy::interpretations(&bytes, cfg, false).expect("a reading");
    let r = readings(&bytes);

    let le = value_of(&r, "u32 (LE)").expect("u32 LE");
    let be = value_of(&r, "u32 (BE)").expect("u32 BE");
    assert!(line.contains(le), "{line} is missing {le}");
    assert!(line.contains(be), "{line} is missing {be}");
    assert_eq!(le, "1684234849");
    assert_eq!(
        u32::from_be_bytes(bytes).to_string(),
        be,
        "the big-endian reading is the one std computes"
    );
    let _ = Endian::Little;
}
