//! Each rendering, and what it falls back to.

use super::*;

fn cell<'a>(raw: &'a str, number: Option<i128>, bytes: &'a [u8]) -> Cell<'a> {
    Cell { raw, number, bytes }
}

fn style(render: Render) -> Style {
    Style {
        render,
        ..Style::default()
    }
}

#[test]
fn a_plain_value_is_the_number_with_its_unit() {
    let mut style = style(Render::Value);
    assert_eq!(style.render(cell("1920", Some(1920), &[])), "1920");
    style.unit = Some("px".to_string());
    assert_eq!(style.render(cell("1920", Some(1920), &[])), "1920 px");
}

#[test]
fn a_scale_multiplies_before_anything_else() {
    let mut style = style(Render::Size);
    style.scale = Some(16384);
    // Two 16 KiB banks of program ROM.
    assert_eq!(style.render(cell("2", Some(2), &[])), "32 KB");
}

#[test]
fn an_enum_names_the_value_and_falls_back_to_the_number() {
    let mut style = style(Render::Enum);
    style.values.insert(6, "RGBA".to_string());
    style.values.insert(0, "greyscale".to_string());
    assert_eq!(style.render(cell("6", Some(6), &[])), "RGBA");
    assert_eq!(style.render(cell("0", Some(0), &[])), "greyscale");
    // No entry for 27: the number, not an invented name and not a blank.
    assert_eq!(style.render(cell("27", Some(27), &[])), "27");
}

#[test]
fn an_enum_with_a_unit_keeps_the_unit_only_on_the_fallback() {
    let mut style = style(Render::Enum);
    style.unit = Some("bit".to_string());
    style.values.insert(8, "eight".to_string());
    assert_eq!(style.render(cell("8", Some(8), &[])), "eight");
    assert_eq!(style.render(cell("9", Some(9), &[])), "9 bit");
}

#[test]
fn flags_list_the_bits_that_are_set() {
    let mut style = style(Render::Flags);
    style.bits.insert(2, "executable".to_string());
    style.bits.insert(8192, "DLL".to_string());
    style.bits.insert(32, "large address aware".to_string());
    assert_eq!(
        style.render(cell("8226", Some(8226), &[])),
        "executable, large address aware, DLL"
    );
    assert_eq!(style.render(cell("2", Some(2), &[])), "executable");
    assert_eq!(style.render(cell("0", Some(0), &[])), "none");
}

#[test]
fn a_flag_mask_of_several_bits_needs_all_of_them() {
    let mut style = style(Render::Flags);
    style.bits.insert(3, "both".to_string());
    assert_eq!(style.render(cell("1", Some(1), &[])), "none");
    assert_eq!(style.render(cell("3", Some(3), &[])), "both");
}

#[test]
fn a_fourcc_is_its_four_characters() {
    let style = style(Render::FourCc);
    assert_eq!(style.render(cell("875967048", None, b"H264")), "H264");
    // Trailing padding is not part of the name.
    assert_eq!(style.render(cell("", None, b"VP8 ")), "VP8");
    // A byte that is not printable does not become a stray glyph.
    assert_eq!(
        style.render(cell("", None, &[0x00, b'a', 0xFF, b'b'])),
        ".a.b"
    );
}

#[test]
fn a_size_reads_in_bytes_then_kb_then_mb() {
    let style = style(Render::Size);
    assert_eq!(style.render(cell("512", Some(512), &[])), "512 B");
    assert_eq!(style.render(cell("1024", Some(1024), &[])), "1.0 KB");
    assert_eq!(style.render(cell("1536", Some(1536), &[])), "1.5 KB");
    assert_eq!(
        style.render(cell("5242880", Some(5_242_880), &[])),
        "5.0 MB"
    );
}

#[test]
fn si_scales_by_thousands_because_a_kilohertz_is_a_thousand() {
    let mut style = style(Render::Si);
    style.unit = Some("Hz".to_string());
    assert_eq!(style.render(cell("44100", Some(44100), &[])), "44.1 kHz");
    assert_eq!(style.render(cell("48000", Some(48000), &[])), "48 kHz");
    assert_eq!(style.render(cell("8000", Some(8000), &[])), "8 kHz");
    assert_eq!(style.render(cell("900", Some(900), &[])), "900 Hz");
    assert_eq!(style.render(cell("2000000", Some(2_000_000), &[])), "2 MHz");
}

#[test]
fn hex_and_time_render_and_fall_back() {
    let hex = style(Render::Hex);
    assert_eq!(
        hex.render(cell("4198400", Some(4_198_400), &[])),
        "0x401000"
    );
    let time = style(Render::Time);
    assert_eq!(
        time.render(cell("1700000000", Some(1_700_000_000), &[])),
        "2023-11-14 22:13:20"
    );
    // Not a plausible time: the number, rather than a date from 1901.
    assert_eq!(time.render(cell("-5", Some(-5), &[])), "-5");
    assert_eq!(
        time.render(cell("99999999999", Some(99_999_999_999), &[])),
        "99999999999"
    );
}

#[test]
fn text_loses_the_quotes_and_the_padding() {
    let style = style(Render::Text);
    assert_eq!(style.render(cell("\"NO NAME    \"", None, &[])), "NO NAME");
    assert_eq!(style.render(cell("\"a.txt...\"", None, &[])), "a.txt");
    assert_eq!(style.render(cell("\"\"", None, &[])), "");
}

#[test]
fn every_rendering_falls_back_to_the_raw_value_when_the_number_is_missing() {
    // What a truncated numeric field looks like: no number at all.
    for render in [
        Render::Value,
        Render::Enum,
        Render::Flags,
        Render::Size,
        Render::Si,
        Render::Hex,
        Render::Time,
    ] {
        let out = style(render).render(cell("(truncated)", None, &[]));
        assert_eq!(out, "(truncated)", "{render:?} invented something");
    }
}

#[test]
fn a_negative_number_is_not_read_as_a_size_or_a_flag_set() {
    // `u64::try_from` refuses it, and the fallback is the number itself.
    assert_eq!(style(Render::Size).render(cell("-1", Some(-1), &[])), "-1");
    assert_eq!(style(Render::Flags).render(cell("-1", Some(-1), &[])), "-1");
}

#[test]
fn the_rendering_names_are_the_ones_the_documentation_lists() {
    for (text, render) in [
        ("value", Render::Value),
        ("text", Render::Text),
        ("enum", Render::Enum),
        ("flags", Render::Flags),
        ("fourcc", Render::FourCc),
        ("size", Render::Size),
        ("si", Render::Si),
        ("hex", Render::Hex),
        ("time", Render::Time),
    ] {
        assert_eq!(Render::parse(text), Some(render));
    }
    assert_eq!(Render::parse("colour"), None);
    assert_eq!(Render::default(), Render::Value);
}
