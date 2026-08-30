//! The template parser, the readings it produces, and the templates the
//! repository ships.

use super::*;
use std::path::{Path, PathBuf};

/// The `templates/` directory of this checkout.
fn shipped_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("templates")
}

const PNG: &str = r#"
name   = "PNG"
endian = "big"
magic  = { offset = 0, bytes = "89504E470D0A1A0A" }

[[field]]
name = "signature"
type = "bytes"
size = 8

[[field]]
name = "ihdr_length"
type = "u32"

[[field]]
name = "ihdr_type"
type = "char"
size = 4

[[field]]
name = "width"
type = "u32"
"#;

/// The first 24 bytes of a 1920-pixel-wide PNG.
fn png_head() -> Vec<u8> {
    let mut bytes = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(&13_u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&1920_u32.to_be_bytes());
    bytes
}

#[test]
fn it_parses_the_shape_from_the_module_documentation() {
    let template = Template::parse(PNG).expect("parses");
    assert_eq!(template.name, "PNG");
    assert_eq!(template.offset, 0);
    assert_eq!(template.span, 20);
    assert_eq!(template.fields.len(), 4);
    let width = template.fields.last().expect("four fields");
    assert_eq!(width.name, "width");
    assert_eq!(width.kind, FieldType::Unsigned(4));
    assert_eq!(width.offset, 16);
    assert_eq!(width.endian, Endian::Big);
}

#[test]
fn it_decodes_each_field_the_way_a_person_reads_it() {
    let template = Template::parse(PNG).expect("parses");
    let readings = applied(&template, &png_head(), 0);
    let values: Vec<(&str, &str)> = readings
        .iter()
        .map(|r| (r.name.as_str(), r.value.as_str()))
        .collect();
    assert_eq!(
        values,
        vec![
            ("signature", "89504E470D0A1A0A"),
            ("ihdr_length", "13"),
            ("ihdr_type", "\"IHDR\""),
            ("width", "1920"),
        ]
    );
    let width = readings.last().expect("four readings");
    assert_eq!((width.offset, width.size), (16, 4));
}

#[test]
fn a_char_field_shows_non_printables_as_dots() {
    let text = r#"
name = "t"
[[field]]
name = "label"
type = "char"
size = 4
"#;
    let template = Template::parse(text).expect("parses");
    let readings = applied(&template, &[b'h', 0x00, b'i', 0xFF], 0);
    assert_eq!(readings.first().map(|r| r.value.as_str()), Some("\"h.i.\""));
}

#[test]
fn the_signed_float_and_unsigned_readings_agree_with_the_inspector() {
    let text = r#"
name   = "t"
endian = "little"
[[field]]
name = "count"
type = "i16"
[[field]]
name = "ratio"
type = "f32"
[[field]]
name = "big"
type = "u64"
"#;
    let template = Template::parse(text).expect("parses");
    let mut bytes = (-2_i16).to_le_bytes().to_vec();
    bytes.extend_from_slice(&1.5_f32.to_le_bytes());
    bytes.extend_from_slice(&u64::MAX.to_le_bytes());
    let readings = applied(&template, &bytes, 0);
    let values: Vec<&str> = readings.iter().map(|r| r.value.as_str()).collect();
    assert_eq!(values, vec!["-2", "1.5", "18446744073709551615"]);
}

#[test]
fn a_per_field_endian_override_beats_the_template_default() {
    let text = r#"
name   = "t"
endian = "little"

[[field]]
name = "little_one"
type = "u16"

[[field]]
name   = "big_one"
type   = "u16"
endian = "big"
"#;
    let template = Template::parse(text).expect("parses");
    assert_eq!(
        template.fields.first().map(|f| f.endian),
        Some(Endian::Little)
    );
    assert_eq!(template.fields.last().map(|f| f.endian), Some(Endian::Big));
    let readings = applied(&template, &[0x01, 0x00, 0x01, 0x00], 0);
    let values: Vec<&str> = readings.iter().map(|r| r.value.as_str()).collect();
    assert_eq!(values, vec!["1", "256"]);
}

#[test]
fn the_default_byte_order_is_little_when_the_template_does_not_say() {
    let template =
        Template::parse("name = \"t\"\n[[field]]\nname=\"n\"\ntype=\"u16\"\n").expect("parses");
    assert_eq!(
        template.fields.first().map(|f| f.endian),
        Some(Endian::Little)
    );
}

#[test]
fn a_declared_offset_may_skip_forward_but_not_backwards() {
    let text = r#"
name = "t"
[[field]]
name = "first"
type = "u8"
[[field]]
name   = "later"
type   = "u8"
offset = 4
"#;
    let template = Template::parse(text).expect("parses");
    assert_eq!(template.span, 5);
    let readings = applied(&template, &[1, 0, 0, 0, 9], 0);
    assert_eq!(readings.get(1).map(|r| r.offset), Some(4));

    let backwards = text.replace("offset = 4", "offset = 0");
    let err = Template::parse(&backwards).expect_err("overlapping fields are refused");
    assert!(
        err.to_string().contains("behind the field before it"),
        "{err}"
    );
}

#[test]
fn applying_at_an_offset_reports_offsets_in_the_same_buffer() {
    let template = Template::parse(PNG).expect("parses");
    let mut bytes = vec![0xEE; 100];
    bytes.extend(png_head());
    let readings = applied(&template, &bytes, 100);
    assert_eq!(readings.first().map(|r| r.offset), Some(100));
    assert_eq!(readings.last().map(|r| r.value.as_str()), Some("1920"));
}

// ---------------------------------------------------------------- failures

#[test]
fn a_bad_type_name_is_refused_by_name() {
    let err = Template::parse("name=\"t\"\n[[field]]\nname=\"n\"\ntype=\"u24\"\n")
        .expect_err("u24 is not a type");
    assert!(
        err.to_string().contains("\"u24\" is not a field type"),
        "{err}"
    );
}

#[test]
fn a_char_field_without_a_size_is_refused() {
    let err = Template::parse("name=\"t\"\n[[field]]\nname=\"n\"\ntype=\"char\"\n")
        .expect_err("char needs a size");
    assert!(err.to_string().contains("needs a size"), "{err}");
    let zero = Template::parse("name=\"t\"\n[[field]]\nname=\"n\"\ntype=\"bytes\"\nsize=0\n")
        .expect_err("a zero size is not a field");
    assert!(zero.to_string().contains("needs a size"), "{zero}");
}

#[test]
fn a_size_on_a_number_is_refused_rather_than_ignored() {
    let err = Template::parse("name=\"t\"\n[[field]]\nname=\"n\"\ntype=\"u32\"\nsize=8\n")
        .expect_err("u32 has its own width");
    assert!(err.to_string().contains("only char and bytes"), "{err}");
}

#[test]
fn a_bad_byte_order_and_a_mistyped_key_are_both_refused() {
    let order = Template::parse("name=\"t\"\nendian=\"middle\"\n").expect_err("no such order");
    assert!(order.to_string().contains("is not a byte order"), "{order}");
    let typo = Template::parse("name=\"t\"\nendain=\"big\"\n").expect_err("unknown key");
    assert!(typo.to_string().contains("endain"), "{typo}");
}

#[test]
fn odd_or_non_hex_magic_digits_are_refused() {
    let odd = Template::parse("name=\"t\"\nmagic={offset=0,bytes=\"ABC\"}\n")
        .expect_err("an odd count is not bytes");
    assert!(odd.to_string().contains("odd number"), "{odd}");
    assert!(Template::parse("name=\"t\"\nmagic={offset=0,bytes=\"ZZ\"}\n").is_err());
}

#[test]
fn whitespace_in_a_magic_is_allowed() {
    let template =
        Template::parse("name=\"t\"\nmagic={offset=1,bytes=\"89 50 4E 47\"}\n").expect("parses");
    assert_eq!(
        template.magic.map(|m| m.bytes),
        Some(vec![0x89, 0x50, 0x4E, 0x47])
    );
}

// -------------------------------------------------------------- truncation

#[test]
fn a_truncated_file_yields_the_fields_that_fit() {
    let template = Template::parse(PNG).expect("parses");
    let head = png_head();
    let short = head.get(..14).expect("14 of 20 bytes");
    let readings = applied(&template, short, 0);
    // signature and ihdr_length are whole; ihdr_type has two of its four
    // bytes; width is not reached at all.
    assert_eq!(readings.len(), 3);
    let last = readings.last().expect("three readings");
    assert_eq!(last.name, "ihdr_type");
    assert_eq!(last.size, 2);
    assert_eq!(last.value, "\"IH\"");
}

#[test]
fn a_number_the_file_ends_inside_says_so_rather_than_guessing() {
    let template = Template::parse(PNG).expect("parses");
    let head = png_head();
    let short = head.get(..10).expect("10 of 20 bytes");
    let readings = applied(&template, short, 0);
    let last = readings.last().expect("two readings");
    assert_eq!((last.name.as_str(), last.size), ("ihdr_length", 2));
    assert_eq!(last.value, "(truncated)");
}

#[test]
fn an_empty_file_and_a_cursor_past_the_end_both_read_as_nothing() {
    let template = Template::parse(PNG).expect("parses");
    assert!(applied(&template, &[], 0).is_empty());
    assert!(applied(&template, &png_head(), 9_000).is_empty());
    assert!(applied(&template, &png_head(), usize::MAX).is_empty());
}

// ------------------------------------------------------------------ magic

#[test]
fn a_wrong_magic_does_not_match_and_a_right_one_does() {
    let template = Template::parse(PNG).expect("parses");
    assert!(matches(&template, &png_head()));
    let mut wrong = png_head();
    if let Some(byte) = wrong.get_mut(3) {
        *byte = 0x00;
    }
    assert!(!matches(&template, &wrong));
    // A file too short to hold the magic is not a match either.
    assert!(!matches(&template, &[0x89, 0x50]));
}

#[test]
fn a_template_without_a_magic_never_claims_a_file() {
    let template =
        Template::parse("name=\"t\"\n[[field]]\nname=\"n\"\ntype=\"u8\"\n").expect("parses");
    assert!(!matches(&template, &[0; 64]));
}

#[test]
fn a_magic_away_from_the_start_is_found_where_it_is() {
    let template =
        Template::parse("name=\"t\"\nmagic={offset=257,bytes=\"7573746172\"}\n").expect("parses");
    let mut block = vec![0_u8; 512];
    if let Some(slot) = block.get_mut(257..262) {
        slot.copy_from_slice(b"ustar");
    }
    assert!(matches(&template, &block));
    assert!(!matches(&template, &vec![0_u8; 512]));
}

// -------------------------------------------------------------- the lookup

/// Three fields with a two-byte gap between the second and the third.
fn covered() -> Vec<FieldReading> {
    let template = Template::parse(
        r#"
name = "t"
[[field]]
name = "a"
type = "u16"
[[field]]
name = "b"
type = "u32"
[[field]]
name   = "c"
type   = "u16"
offset = 8
"#,
    )
    .expect("parses");
    applied(&template, &[0; 16], 0)
}

#[test]
fn field_at_finds_the_field_a_byte_is_inside() {
    let readings = covered();
    assert_eq!(field_at(&readings, 3).map(|r| r.name.as_str()), Some("b"));
}

#[test]
fn field_at_finds_the_first_and_last_byte_of_a_field() {
    let readings = covered();
    assert_eq!(field_at(&readings, 2).map(|r| r.name.as_str()), Some("b"));
    assert_eq!(field_at(&readings, 5).map(|r| r.name.as_str()), Some("b"));
    assert_eq!(field_at(&readings, 0).map(|r| r.name.as_str()), Some("a"));
    assert_eq!(field_at(&readings, 9).map(|r| r.name.as_str()), Some("c"));
}

#[test]
fn field_at_answers_nothing_in_a_gap_and_past_the_end() {
    let readings = covered();
    assert!(field_at(&readings, 6).is_none());
    assert!(field_at(&readings, 7).is_none());
    assert!(field_at(&readings, 10).is_none());
    assert!(field_at(&readings, u64::MAX).is_none());
    assert!(field_at::<FieldReading>(&[], 0).is_none());
}

#[test]
fn field_at_agrees_with_a_scan_over_every_byte() {
    let readings = covered();
    for offset in 0..16_u64 {
        let scanned = readings.iter().find(|r| r.covers(offset));
        assert_eq!(field_at(&readings, offset), scanned, "at {offset}");
    }
}

// ------------------------------------------------------ the shipped set

#[test]
fn every_shipped_template_parses() {
    let (templates, problems) = load_dir(&shipped_dir());
    assert!(problems.is_empty(), "{problems:#?}");
    assert!(
        templates.len() > 80,
        "only {} templates were found",
        templates.len()
    );
    // Sorted by name, and no two share one, or the picker would offer the
    // same row twice and mean different things by it.
    let mut names: Vec<&str> = templates.iter().map(|t| t.name.as_str()).collect();
    let before = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), before, "two templates share a name");
    // Every field of every template is inside its own declared span, which is
    // what the walk in `Template::parse` maintains.
    for template in &templates {
        for field in &template.fields {
            assert!(
                field.offset.saturating_add(field.size) <= template.span,
                "{}: {} runs past the span",
                template.name,
                field.name
            );
            assert!(field.size > 0, "{}: {} is empty", template.name, field.name);
        }
    }
}

/// One template found by name, or a failure that says which is missing.
fn shipped(name: &str) -> Template {
    let (templates, _) = load_dir(&shipped_dir());
    templates
        .into_iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("no shipped template called {name:?}"))
}

/// Where a field of a shipped template starts and how wide it is.
fn field(template: &Template, name: &str) -> (usize, usize) {
    template
        .fields
        .iter()
        .find(|f| f.name == name)
        .map(|f| (f.offset, f.size))
        .unwrap_or_else(|| panic!("{} has no field {name:?}", template.name))
}

/// Every declared magic matches a buffer built to hold exactly it.
///
/// A synthetic buffer rather than a real file: the point is that the offset
/// and the bytes agree with each other and with [`matches`], and a zeroed
/// buffer with the magic spliced in tests that without needing a hundred
/// sample files in the repository.
#[test]
fn every_declared_magic_matches_bytes_built_to_hold_it() {
    let (templates, _) = load_dir(&shipped_dir());
    let mut with_magic = 0;
    for template in &templates {
        let Some(magic) = template.magic.as_ref() else {
            continue;
        };
        with_magic += 1;
        assert!(!magic.bytes.is_empty(), "{}: an empty magic", template.name);
        let end = magic.offset + magic.bytes.len();
        let mut buffer = vec![0_u8; end + 8];
        buffer[magic.offset..end].copy_from_slice(&magic.bytes);
        assert!(
            matches(template, &buffer),
            "{} does not match",
            template.name
        );
        // One byte out and it must stop matching, which is what says the
        // offset in the file is the one being tested.
        buffer[magic.offset] ^= 0xFF;
        assert!(
            !matches(template, &buffer),
            "{} matches a corrupted magic",
            template.name
        );
    }
    assert!(
        with_magic > 70,
        "only {with_magic} templates declare a magic"
    );
}

/// Handcrafted heads for the formats a person is most likely to open, with
/// the values their fields must read.
#[test]
fn the_everyday_formats_read_a_handcrafted_header_correctly() {
    // PNG: the signature, then an IHDR chunk for a 1920x1080 image.
    let mut png = png_head();
    png.extend_from_slice(&1080_u32.to_be_bytes());
    png.extend_from_slice(&[8, 6, 0, 0, 0]);
    png.extend_from_slice(&0_u32.to_be_bytes());
    let template = shipped("PNG");
    assert!(matches(&template, &png));
    let readings = applied(&template, &png, 0);
    let value = |name: &str| {
        readings
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.value.clone())
            .unwrap_or_default()
    };
    assert_eq!(value("width"), "1920");
    assert_eq!(value("height"), "1080");
    assert_eq!(value("bit_depth"), "8");
    assert_eq!(value("ihdr_type"), "\"IHDR\"");

    // GIF: GIF89a, 320x200.
    let mut gif = b"GIF89a".to_vec();
    gif.extend_from_slice(&320_u16.to_le_bytes());
    gif.extend_from_slice(&200_u16.to_le_bytes());
    gif.extend_from_slice(&[0xF7, 0x00, 0x00]);
    let template = shipped("GIF header");
    assert!(matches(&template, &gif));
    let readings = applied(&template, &gif, 0);
    assert_eq!(
        readings
            .iter()
            .find(|r| r.name == "width")
            .map(|r| r.value.as_str()),
        Some("320")
    );
    assert_eq!(
        readings
            .iter()
            .find(|r| r.name == "version")
            .map(|r| r.value.as_str()),
        Some("\"89a\"")
    );

    // ELF64: a little-endian x86-64 executable.
    let mut elf = vec![0x7F, b'E', b'L', b'F', 2, 1, 1, 0, 0];
    elf.extend_from_slice(&[0; 7]);
    elf.extend_from_slice(&2_u16.to_le_bytes()); // e_type: ET_EXEC
    elf.extend_from_slice(&0x3E_u16.to_le_bytes()); // e_machine: x86-64
    elf.extend_from_slice(&1_u32.to_le_bytes());
    elf.extend_from_slice(&0x40_1000_u64.to_le_bytes()); // e_entry
    elf.extend_from_slice(&64_u64.to_le_bytes()); // e_phoff
    elf.resize(64, 0);
    let template = shipped("ELF64 header");
    assert!(matches(&template, &elf));
    assert_eq!(template.span, 64, "the ELF64 header is 64 bytes");
    let readings = applied(&template, &elf, 0);
    let value = |name: &str| {
        readings
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.value.clone())
            .unwrap_or_default()
    };
    assert_eq!(value("e_machine"), "62");
    assert_eq!(value("e_entry"), "4198400");
    assert_eq!(value("e_phoff"), "64");
    // An ELF32 file must not match the 64-bit template, and the other way
    // round: the class byte is part of both magics.
    let mut elf32 = elf.clone();
    elf32[4] = 1;
    assert!(!matches(&template, &elf32));
    assert!(matches(&shipped("ELF32 header"), &elf32));

    // ZIP: a stored member of 100 bytes called "a.txt".
    let mut zip = vec![0x50, 0x4B, 0x03, 0x04];
    zip.extend_from_slice(&20_u16.to_le_bytes());
    zip.extend_from_slice(&0_u16.to_le_bytes());
    zip.extend_from_slice(&0_u16.to_le_bytes());
    zip.extend_from_slice(&[0; 4]); // time and date
    zip.extend_from_slice(&0_u32.to_le_bytes()); // crc
    zip.extend_from_slice(&100_u32.to_le_bytes());
    zip.extend_from_slice(&100_u32.to_le_bytes());
    zip.extend_from_slice(&5_u16.to_le_bytes());
    zip.extend_from_slice(&0_u16.to_le_bytes());
    zip.extend_from_slice(b"a.txt");
    let template = shipped("ZIP local file header");
    assert!(matches(&template, &zip));
    assert_eq!(template.span, 30, "a local file header is 30 bytes");
    let readings = applied(&template, &zip, 0);
    assert_eq!(
        readings
            .iter()
            .find(|r| r.name == "uncompressed_size")
            .map(|r| r.value.as_str()),
        Some("100")
    );
    assert_eq!(
        readings
            .iter()
            .find(|r| r.name == "file_name_length")
            .map(|r| r.value.as_str()),
        Some("5")
    );

    // GZIP: deflate, with a modification time.
    let mut gzip = vec![0x1F, 0x8B, 0x08, 0x00];
    gzip.extend_from_slice(&1_700_000_000_u32.to_le_bytes());
    gzip.extend_from_slice(&[0x00, 0x03]);
    let template = shipped("GZIP");
    assert!(matches(&template, &gzip));
    let readings = applied(&template, &gzip, 0);
    assert_eq!(
        readings
            .iter()
            .find(|r| r.name == "mtime")
            .map(|r| r.value.as_str()),
        Some("1700000000")
    );

    // SQLite: the header string and a 4096-byte page size, big-endian.
    let mut sqlite = b"SQLite format 3\0".to_vec();
    sqlite.extend_from_slice(&4096_u16.to_be_bytes());
    sqlite.resize(100, 0);
    let template = shipped("SQLite 3");
    assert!(matches(&template, &sqlite));
    let readings = applied(&template, &sqlite, 0);
    assert_eq!(
        readings
            .iter()
            .find(|r| r.name == "page_size")
            .map(|r| r.value.as_str()),
        Some("4096")
    );
}

/// The tar template's offsets are the ones the working reader uses.
///
/// `src/vfs/archive/tar.rs` reads real archives and is tested against them, so
/// where it and a template could disagree, it is right. These are its own
/// constants, spelled out here so a template edit that moved a field would
/// fail rather than quietly mislabel a header.
#[test]
fn the_tar_template_agrees_with_the_working_tar_reader() {
    let template = shipped("TAR (ustar)");
    assert_eq!(field(&template, "size"), (124, 12));
    assert_eq!(field(&template, "checksum"), (148, 8));
    assert_eq!(field(&template, "typeflag"), (156, 1));
    assert_eq!(field(&template, "magic"), (257, 6));
    assert_eq!(field(&template, "prefix"), (345, 155));

    // A header block built the way the reader expects one.
    let mut block = vec![0_u8; 512];
    block[..5].copy_from_slice(b"a.txt");
    block[124..135].copy_from_slice(b"00000000144");
    block[156] = b'0';
    block[257..263].copy_from_slice(b"ustar\0");
    assert!(matches(&template, &block));
    let readings = applied(&template, &block, 0);
    let value = |name: &str| {
        readings
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.value.clone())
            .unwrap_or_default()
    };
    // Octal ASCII, not a number: 0o144 is 100 bytes, and the template says so
    // by leaving it as text rather than pretending it is an integer.
    assert_eq!(value("size"), "\"00000000144.\"");
    assert_eq!(value("typeflag"), "\"0\"");
    assert!(value("name").starts_with("\"a.txt"));
}

/// The ISO 9660 template's descriptor is where the working reader looks.
///
/// `src/vfs/image/iso.rs` reads the first descriptor at sector 16 of a
/// 2048-byte-sector volume and checks for `CD001` at offset 1 of it.
#[test]
fn the_iso_template_agrees_with_the_working_iso_reader() {
    let template = shipped("ISO 9660 PVD");
    assert_eq!(template.offset, 16 * 2048);
    assert_eq!(field(&template, "standard_identifier"), (1, 5));
    assert_eq!(field(&template, "logical_block_size_le"), (128, 2));

    let mut image = vec![0_u8; 32768 + 2048];
    image[32768] = 1; // a primary volume descriptor
    image[32769..32774].copy_from_slice(b"CD001");
    image[32775] = 1; // version
    image[32768 + 40..32768 + 45].copy_from_slice(b"MYCD ");
    image[32768 + 128..32768 + 130].copy_from_slice(&2048_u16.to_le_bytes());
    image[32768 + 130..32768 + 132].copy_from_slice(&2048_u16.to_be_bytes());
    assert!(matches(&template, &image));

    let readings = applied(&template, &image, template.offset);
    let value = |name: &str| {
        readings
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.value.clone())
            .unwrap_or_default()
    };
    assert_eq!(value("standard_identifier"), "\"CD001\"");
    // The same number written both ways round, which is what the per-field
    // endian overrides are for.
    assert_eq!(value("logical_block_size_le"), "2048");
    assert_eq!(value("logical_block_size_be"), "2048");
    assert!(value("volume_identifier").starts_with("\"MYCD "));
}

/// The MBR and GPT templates against a table built the way a real disk has one.
#[test]
fn the_partition_templates_read_a_handcrafted_disk() {
    let mut disk = vec![0_u8; 1024];
    // One partition: bootable, type 0x83, starting at LBA 2048.
    disk[446] = 0x80;
    disk[450] = 0x83;
    disk[454..458].copy_from_slice(&2048_u32.to_le_bytes());
    disk[458..462].copy_from_slice(&20480_u32.to_le_bytes());
    disk[510..512].copy_from_slice(&[0x55, 0xAA]);
    let mbr = shipped("MBR");
    assert!(matches(&mbr, &disk));
    assert_eq!(mbr.span, 512, "an MBR is one 512-byte sector");
    assert_eq!(field(&mbr, "p1_status"), (446, 1));
    assert_eq!(field(&mbr, "p4_sectors"), (506, 4));
    let readings = applied(&mbr, &disk, 0);
    let value = |name: &str| {
        readings
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.value.clone())
            .unwrap_or_default()
    };
    assert_eq!(value("p1_status"), "128");
    assert_eq!(value("p1_lba_first"), "2048");
    assert_eq!(value("p1_sectors"), "20480");
    assert_eq!(value("signature"), "55AA");

    // The GPT header is in the sector after it.
    disk[512..520].copy_from_slice(b"EFI PART");
    disk[520..524].copy_from_slice(&0x0001_0000_u32.to_le_bytes());
    disk[524..528].copy_from_slice(&92_u32.to_le_bytes());
    disk[584..592].copy_from_slice(&2_u64.to_le_bytes());
    disk[592..596].copy_from_slice(&128_u32.to_le_bytes());
    disk[596..600].copy_from_slice(&128_u32.to_le_bytes());
    let gpt = shipped("GPT header");
    assert_eq!(gpt.offset, 512);
    assert!(matches(&gpt, &disk));
    let readings = applied(&gpt, &disk, gpt.offset);
    let value = |name: &str| {
        readings
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.value.clone())
            .unwrap_or_default()
    };
    assert_eq!(value("signature"), "\"EFI PART\"");
    assert_eq!(value("header_size"), "92");
    assert_eq!(value("partition_entry_lba"), "2");
    assert_eq!(value("number_of_partition_entries"), "128");
    assert_eq!(value("size_of_partition_entry"), "128");
}

/// The FAT and ext templates, at the offsets those filesystems put them at.
#[test]
fn the_filesystem_templates_read_a_handcrafted_superblock() {
    // FAT32: a BPB with 512-byte sectors and eight per cluster.
    let mut volume = vec![0_u8; 512];
    volume[..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
    volume[3..11].copy_from_slice(b"mkfs.fat");
    volume[11..13].copy_from_slice(&512_u16.to_le_bytes());
    volume[13] = 8;
    volume[14..16].copy_from_slice(&32_u16.to_le_bytes());
    volume[16] = 2;
    volume[36..40].copy_from_slice(&1024_u32.to_le_bytes());
    volume[44..48].copy_from_slice(&2_u32.to_le_bytes());
    volume[71..82].copy_from_slice(b"NO NAME    ");
    volume[82..90].copy_from_slice(b"FAT32   ");
    volume[510..512].copy_from_slice(&[0x55, 0xAA]);
    let fat = shipped("FAT32 BPB");
    assert!(matches(&fat, &volume));
    assert_eq!(field(&fat, "bytes_per_sector"), (11, 2));
    assert_eq!(field(&fat, "root_cluster"), (44, 4));
    let readings = applied(&fat, &volume, 0);
    let value = |name: &str| {
        readings
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.value.clone())
            .unwrap_or_default()
    };
    assert_eq!(value("bytes_per_sector"), "512");
    assert_eq!(value("sectors_per_cluster"), "8");
    assert_eq!(value("num_fats"), "2");
    assert_eq!(value("sectors_per_fat_32"), "1024");
    assert_eq!(value("fs_type"), "\"FAT32   \"");

    // ext4: the superblock is at 1024 and its magic sixteen bytes further on.
    let mut volume = vec![0_u8; 2048];
    volume[1024..1028].copy_from_slice(&65_536_u32.to_le_bytes());
    volume[1024 + 24..1024 + 28].copy_from_slice(&2_u32.to_le_bytes());
    volume[1024 + 56..1024 + 58].copy_from_slice(&0xEF53_u16.to_le_bytes());
    volume[1024 + 88..1024 + 90].copy_from_slice(&256_u16.to_le_bytes());
    volume[1024 + 120..1024 + 126].copy_from_slice(b"backup");
    let ext = shipped("ext2/3/4 superblock");
    assert_eq!(ext.offset, 1024);
    assert!(matches(&ext, &volume));
    let readings = applied(&ext, &volume, ext.offset);
    let value = |name: &str| {
        readings
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.value.clone())
            .unwrap_or_default()
    };
    assert_eq!(value("s_inodes_count"), "65536");
    assert_eq!(value("s_magic"), "61267");
    assert_eq!(value("s_log_block_size"), "2");
    assert_eq!(value("s_inode_size"), "256");
    assert!(value("s_volume_name").starts_with("\"backup"));
    // The reading's offsets are absolute, so the volume name really is where
    // the template says it is.
    assert_eq!(
        readings
            .iter()
            .find(|r| r.name == "s_volume_name")
            .map(|r| r.offset),
        Some(1024 + 120)
    );
}

#[test]
fn the_shipped_set_covers_the_formats_a_hex_viewer_is_opened_for() {
    let (templates, _) = load_dir(&shipped_dir());
    let names: Vec<&str> = templates.iter().map(|t| t.name.as_str()).collect();
    for wanted in [
        "PNG",
        "JPEG (JFIF)",
        "GIF header",
        "BMP",
        "ELF64 header",
        "ELF32 header",
        "DOS MZ header",
        "PE header (PE32+)",
        "Mach-O 64 header",
        "PDF header",
        "ZIP local file header",
        "GZIP",
        "SQLite 3",
        "MBR",
        "GPT header",
        "FAT32 BPB",
        "NTFS boot sector",
        "ext2/3/4 superblock",
        "ISO 9660 PVD",
        "TAR (ustar)",
        "WAV (RIFF)",
        "Android DEX",
        "Java class",
        "Python pyc (3.7+)",
        "TrueType (sfnt)",
        "Windows LNK",
        "Registry hive (regf)",
        "Binary plist",
        "QCOW2",
        "Device tree blob",
    ] {
        assert!(names.contains(&wanted), "{wanted} is not shipped");
    }
}

#[test]
fn a_directory_that_is_not_there_yields_nothing_rather_than_failing() {
    let (templates, problems) = load_dir(Path::new("/no/such/directory"));
    assert!(templates.is_empty());
    assert!(problems.is_empty());
}

#[test]
fn a_template_that_will_not_parse_is_reported_and_the_rest_still_load() {
    let dir = std::env::temp_dir().join(format!("hcmd-templates-{}", std::process::id()));
    let nested = dir.join("nested");
    std::fs::create_dir_all(&nested).expect("temp dir");
    std::fs::write(dir.join("good.toml"), "name = \"Good\"\n").expect("write");
    std::fs::write(nested.join("deep.toml"), "name = \"Deep\"\n").expect("write");
    std::fs::write(dir.join("bad.toml"), "name = 3\n").expect("write");
    std::fs::write(dir.join("ignored.txt"), "not a template").expect("write");

    let (templates, problems) = load_dir(&dir);
    let names: Vec<&str> = templates.iter().map(|t| t.name.as_str()).collect();
    // The subdirectory is walked, so the grouping in `templates/` is invisible
    // to the picker.
    assert_eq!(names, vec!["Deep", "Good"]);
    assert_eq!(problems.len(), 1);
    assert!(problems.first().is_some_and(|p| p.contains("bad.toml")));
    std::fs::remove_dir_all(&dir).ok();
}

// ------------------------------------------------------- extents and coverage

/// A template of three fields with a two-byte gap before the third.
fn spaced() -> Template {
    Template::parse(
        r#"
name = "t"
[[field]]
name = "a"
type = "u16"
[[field]]
name = "b"
type = "u32"
[[field]]
name   = "c"
type   = "u16"
offset = 8
"#,
    )
    .expect("parses")
}

#[test]
fn extents_place_every_field_without_reading_a_byte() {
    let spans = extents(&spaced(), 0, 1000);
    let placed: Vec<(&str, u64, usize)> = spans
        .iter()
        .map(|s| (s.name.as_str(), s.offset, s.size))
        .collect();
    assert_eq!(placed, vec![("a", 0, 2), ("b", 2, 4), ("c", 8, 2)]);
}

#[test]
fn extents_are_relative_to_where_the_template_is_applied() {
    let spans = extents(&spaced(), 0x40, 1000);
    assert_eq!(spans.first().map(|s| s.offset), Some(0x40));
    assert_eq!(spans.last().map(|s| s.offset), Some(0x48));
}

#[test]
fn extents_never_point_past_the_end_of_the_file() {
    // The file ends in the middle of `b`.
    let spans = extents(&spaced(), 0, 5);
    assert_eq!(spans.len(), 2);
    assert_eq!(spans.last().map(|s| (s.offset, s.size)), Some((2, 3)));
    // And a template applied past the end places nothing at all.
    assert!(extents(&spaced(), 5000, 100).is_empty());
    assert!(extents(&spaced(), 0, 0).is_empty());
}

#[test]
fn field_at_works_on_extents_as_well_as_on_readings() {
    let spans = extents(&spaced(), 0, 1000);
    assert_eq!(field_at(&spans, 3).map(|s| s.name.as_str()), Some("b"));
    assert_eq!(field_at(&spans, 9).map(|s| s.name.as_str()), Some("c"));
    // The gap at 6 and 7 belongs to nothing, and neither does 10.
    assert!(field_at(&spans, 6).is_none());
    assert!(field_at(&spans, 10).is_none());
}

#[test]
fn coverage_merges_fields_that_touch_and_keeps_the_gap() {
    let spans = extents(&spaced(), 0, 1000);
    // a and b abut, so they are one run; c is separate.
    assert_eq!(coverage(&spans, 0, 16), vec![0..6, 8..10]);
}

#[test]
fn coverage_is_relative_to_the_row_and_clipped_to_it() {
    let spans = extents(&spaced(), 0, 1000);
    // A row starting at 4: the tail of b, then c.
    assert_eq!(coverage(&spans, 4, 8), vec![0..2, 4..6]);
    // A row that ends inside a field.
    assert_eq!(coverage(&spans, 0, 4), vec![0..4]);
    // A row entirely inside one field is covered from end to end.
    let wide = extents(
        &Template::parse("name=\"t\"\n[[field]]\nname=\"big\"\ntype=\"bytes\"\nsize=64\n")
            .expect("parses"),
        0,
        1000,
    );
    assert_eq!(coverage(&wide, 16, 16), vec![0..16]);
}

#[test]
fn coverage_of_a_row_before_and_after_everything_is_empty() {
    let spans = extents(&spaced(), 100, 1000);
    assert!(coverage(&spans, 0, 16).is_empty());
    assert!(coverage(&spans, 500, 16).is_empty());
    assert!(coverage(&[] as &[FieldSpan], 0, 16).is_empty());
    assert!(coverage(&spans, 0, 0).is_empty());
}

/// The binary search must find the same rows a scan would.
#[test]
fn coverage_agrees_with_a_scan_over_every_row_of_a_long_structure() {
    let mut text = String::from("name = \"long\"\n");
    for i in 0..120 {
        text.push_str(&format!("\n[[field]]\nname = \"f{i}\"\ntype = \"u32\"\n"));
    }
    let template = Template::parse(&text).expect("parses");
    let spans = extents(&template, 7, 10_000);
    for row in 0..40_u64 {
        let from = row.saturating_mul(16);
        let got = coverage(&spans, from, 16);
        // What a scan of every byte of the row would say.
        let mut want: Vec<std::ops::Range<usize>> = Vec::new();
        for i in 0..16_usize {
            let at = from.saturating_add(i as u64);
            if spans.iter().any(|s| s.covers_offset(at)) {
                match want.last_mut() {
                    Some(last) if last.end == i => last.end = i + 1,
                    _ => want.push(i..i + 1),
                }
            }
        }
        assert_eq!(got, want, "row at {from}");
    }
}

// ------------------------------------------- the automatic match in hex

/// A viewer over `body`, named `name`, laid out ready to draw.
fn hex_viewer(name: &str, body: &[u8]) -> crate::viewer::Viewer {
    let bytes = std::sync::Arc::new(body.to_vec());
    let len = bytes.len() as u64;
    let mut viewer = crate::viewer::Viewer::open(
        crate::viewer::ViewerId(1),
        name,
        None,
        crate::viewer::source::memory_opener(bytes),
        Some(len),
        &crate::config::ViewerConfig::default(),
    )
    .expect("open");
    // The way the event loop opens one: the file is recognised from its head
    // once, here, and hex mode then has a template without reading anything.
    let cfg = crate::config::ViewerConfig::default();
    viewer
        .choose_initial_mode(cfg.default_mode, cfg.open_as_document)
        .expect("choose");
    viewer
        .set_mode(crate::config::ViewerMode::Hex)
        .expect("hex");
    viewer.layout(10, 80).expect("layout");
    viewer
}

/// A whole PNG head: signature, IHDR, 1920x1080 RGBA.
fn png_file() -> Vec<u8> {
    let mut png = png_head();
    png.extend_from_slice(&1080_u32.to_be_bytes());
    png.extend_from_slice(&[8, 6, 0, 0, 0]);
    png.extend_from_slice(&0_u32.to_be_bytes());
    png.resize(256, 0);
    png
}

#[test]
fn entering_hex_colours_a_recognised_file_with_no_key_pressed() {
    let viewer = hex_viewer("shot.png", &png_file());
    assert_eq!(viewer.template_name(), Some("PNG"));
    assert_eq!(viewer.template_pick(), crate::viewer::TemplatePick::Auto);
    // And the spans are built in the same layout, not a frame later.
    assert!(!viewer.template_spans().is_empty());
}

#[test]
fn a_file_nothing_recognises_gets_no_template_and_says_nothing_about_it() {
    let viewer = hex_viewer("notes.txt", b"just some plain text, nothing to match\n");
    assert_eq!(viewer.template_name(), None);
    assert!(viewer.template_spans().is_empty());
    assert_eq!(viewer.field_reading(), None);
}

#[test]
fn a_signature_past_the_screen_is_still_matched_because_the_head_is_read_at_open() {
    // A FAT32 boot sector: the 0x55AA at 510 is past what a ten-row screen of
    // sixteen bytes would show, and is found anyway - because the recognition
    // happens once when the file is opened, over a fixed head, and not from
    // whatever the terminal's height happened to put on screen.
    let mut volume = vec![0_u8; 1024];
    volume[..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
    volume[11..13].copy_from_slice(&512_u16.to_le_bytes());
    volume[82..90].copy_from_slice(b"FAT32   ");
    volume[510..512].copy_from_slice(&[0x55, 0xAA]);
    let viewer = hex_viewer("disk.img", &volume);
    assert!(
        viewer.template_name().is_some(),
        "nothing matched a boot sector"
    );
}

#[test]
fn a_chosen_template_survives_leaving_hex_mode_and_coming_back() {
    let mut viewer = hex_viewer("shot.png", &png_file());
    // The user overrides the automatic match with something else.
    let gif = shipped("GIF header");
    viewer.set_template(Some(gif));
    assert_eq!(viewer.template_pick(), crate::viewer::TemplatePick::Chosen);

    viewer
        .set_mode(crate::config::ViewerMode::Text)
        .expect("text");
    viewer
        .set_mode(crate::config::ViewerMode::Hex)
        .expect("hex");
    viewer.layout(10, 80).expect("layout");
    assert_eq!(
        viewer.template_name(),
        Some("GIF header"),
        "the automatic match replaced a choice"
    );
    assert_eq!(viewer.template_pick(), crate::viewer::TemplatePick::Chosen);
}

#[test]
fn choosing_none_survives_a_mode_switch_and_stays_off() {
    let mut viewer = hex_viewer("shot.png", &png_file());
    assert_eq!(viewer.template_name(), Some("PNG"));
    // The user turns the colouring off deliberately.
    viewer.set_template(None);
    assert_eq!(viewer.template_pick(), crate::viewer::TemplatePick::Refused);

    viewer
        .set_mode(crate::config::ViewerMode::Text)
        .expect("text");
    viewer
        .set_mode(crate::config::ViewerMode::Hex)
        .expect("hex");
    viewer.layout(10, 80).expect("layout");
    assert_eq!(
        viewer.template_name(),
        None,
        "turning it off did not stay off"
    );
    assert!(viewer.template_spans().is_empty());
}

#[test]
fn the_match_runs_once_and_not_again_on_every_frame() {
    let mut viewer = hex_viewer("shot.png", &png_file());
    viewer.set_template(None);
    for _ in 0..5 {
        viewer.layout(10, 80).expect("layout");
    }
    assert_eq!(
        viewer.template_name(),
        None,
        "it matched again behind a refusal"
    );
}

// ------------------------------------- what the cursor is standing in

/// The status line's field reading with the cursor at `at`.
fn reading_at(viewer: &mut crate::viewer::Viewer, at: u64) -> Option<String> {
    viewer.place_cursor(at).expect("place");
    viewer.layout(10, 80).expect("layout");
    viewer.field_reading().map(str::to_string)
}

#[test]
fn the_status_line_names_the_field_and_what_it_says() {
    let mut viewer = hex_viewer("shot.png", &png_file());
    // A number: the width, at 16.
    assert_eq!(reading_at(&mut viewer, 16).as_deref(), Some("width: 1920"));
    // Every byte of it reads the same, not just its first.
    assert_eq!(reading_at(&mut viewer, 19).as_deref(), Some("width: 1920"));
    // A one-byte number.
    assert_eq!(reading_at(&mut viewer, 24).as_deref(), Some("bit_depth: 8"));
    // A string: the chunk type.
    assert_eq!(
        reading_at(&mut viewer, 12).as_deref(),
        Some("ihdr_type: \"IHDR\"")
    );
    // Bytes: the signature, as hex.
    assert_eq!(
        reading_at(&mut viewer, 0).as_deref(),
        Some("signature: 89504E470D0A1A0A")
    );
}

#[test]
fn outside_every_field_the_status_line_says_nothing() {
    let mut viewer = hex_viewer("shot.png", &png_file());
    // The PNG template spans 33 bytes; past it is a gap.
    assert_eq!(reading_at(&mut viewer, 40), None);
    assert_eq!(reading_at(&mut viewer, 200), None);
}

#[test]
fn a_gap_between_two_fields_reads_as_nothing_rather_than_as_a_neighbour() {
    let text = r#"
name = "gapped"
magic = { offset = 0, bytes = "AA" }

[[field]]
name = "first"
type = "u16"

[[field]]
name   = "second"
type   = "u16"
offset = 8
"#;
    let mut viewer = hex_viewer("x.bin", &[0xAA; 64]);
    viewer.set_template(Some(Template::parse(text).expect("parses")));
    assert_eq!(reading_at(&mut viewer, 0).as_deref(), Some("first: 43690"));
    // Bytes 2 to 7 belong to nothing.
    for at in 2..8 {
        assert_eq!(reading_at(&mut viewer, at), None, "byte {at} is in a gap");
    }
    assert_eq!(reading_at(&mut viewer, 8).as_deref(), Some("second: 43690"));
}

/// Picking a template by hand anchors it at the cursor, because picking one
/// is the act of saying "read the bytes *here* as this".
#[test]
fn a_hand_picked_template_is_anchored_where_the_cursor_was() {
    let mut viewer = hex_viewer("x.bin", &png_file());
    viewer.place_cursor(100).expect("place");
    viewer.set_template(Some(shipped("PNG")));
    assert_eq!(viewer.template_at(), Some(100));
    viewer.layout(10, 80).expect("layout");
    assert_eq!(
        viewer.field_reading(),
        Some("signature: 0000000000000000"),
        "the first field is where the cursor was"
    );
    // And it stays there when the cursor moves on.
    assert_eq!(
        reading_at(&mut viewer, 108).as_deref(),
        Some("ihdr_length: 0")
    );
    assert_eq!(viewer.template_at(), Some(100), "the anchor did not move");
}

/// An automatic match is anchored where the template says its structure is,
/// which for a PNG is the front of the file and not wherever the cursor sat.
#[test]
fn an_automatic_match_is_anchored_at_the_templates_own_offset() {
    let viewer = hex_viewer("shot.png", &png_file());
    assert_eq!(viewer.template_at(), Some(0));
}

/// A magic of one byte matches one file in 256, so it may not claim a file
/// nobody asked about. DER's is `0x30`, which is also the digit `0`.
#[test]
fn a_one_byte_magic_does_not_claim_a_file_unprompted() {
    let viewer = hex_viewer("numbers.txt", b"0123456789abcdef");
    assert_eq!(
        viewer.template_name(),
        None,
        "a text file starting with a zero was labelled a certificate"
    );
    // The picker still applies it by hand, which is a different question.
    assert_eq!(
        crate::viewer::summary::best_match(crate::viewer::fileinfo::builtin(), b"0123456789abcdef")
            .map(|t| t.name.as_str()),
        Some("DER sequence"),
        "the template itself is right; only the automatic path is fussier"
    );
}

#[test]
fn the_evidence_bar_is_two_bytes_and_is_applied_to_every_automatic_path() {
    use crate::viewer::summary::{AUTO_MATCH_MIN_MAGIC, auto_match, best_match};
    assert_eq!(AUTO_MATCH_MIN_MAGIC, 2);
    let one = Template::parse("name=\"one\"\nmagic={offset=0,bytes=\"30\"}\n").expect("a");
    let two = Template::parse("name=\"two\"\nmagic={offset=0,bytes=\"3031\"}\n").expect("b");
    let set = vec![one, two];
    // Both claim these bytes; only the two-byte one may say so unprompted.
    assert_eq!(
        best_match(&set, b"01xx").map(|t| t.name.as_str()),
        Some("two")
    );
    assert_eq!(
        auto_match(&set, b"01xx").map(|t| t.name.as_str()),
        Some("two")
    );
    // Where only the one-byte magic matches, nothing is claimed.
    assert_eq!(
        best_match(&set, b"0zzz").map(|t| t.name.as_str()),
        Some("one")
    );
    assert_eq!(auto_match(&set, b"0zzz"), None);
}
