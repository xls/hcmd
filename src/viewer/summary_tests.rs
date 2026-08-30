//! The `[summary]` section: parsing it, and what the shipped ones say about a
//! handcrafted header.

use super::*;
use crate::viewer::fileinfo::builtin;
use crate::viewer::template::matches;

/// The shipped template of that name, or a failure naming it.
fn shipped(name: &str) -> &'static Template {
    builtin()
        .iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("no shipped template called {name:?}"))
}

/// Every summary line of `template` applied to `bytes`, as label and value.
fn lines(name: &str, bytes: &[u8]) -> Vec<(String, String)> {
    let template = shipped(name);
    assert!(
        matches(template, bytes),
        "{name} does not recognise these bytes"
    );
    summary(template, bytes)
        .into_iter()
        .map(|line| (line.label, line.value))
        .collect()
}

/// One line's value, or a failure naming the missing label.
fn value(lines: &[(String, String)], label: &str) -> String {
    lines
        .iter()
        .find(|(name, _)| name == label)
        .map(|(_, value)| value.clone())
        .unwrap_or_else(|| panic!("no summary line called {label:?} in {lines:#?}"))
}

const TEMPLATE: &str = r#"
name   = "T"
endian = "little"

[[field]]
name = "width"
type = "u16"

[[field]]
name = "height"
type = "u16"

[[field]]
name = "kind"
type = "u8"

[summary]
title = "A thing"

[[summary.line]]
label = "Dimensions"
field = "width"
with  = "height"
join  = " x "
unit  = "px"

[[summary.line]]
label  = "Kind"
field  = "kind"
render = "enum"
enum   = { 1 = "first", 2 = "second" }
"#;

#[test]
fn a_summary_is_parsed_and_rendered() {
    let template = Template::parse(TEMPLATE).expect("parses");
    assert_eq!(heading(&template), "A thing");
    let bytes = [0x80, 0x07, 0x38, 0x04, 0x02];
    let out = summary(&template, &bytes);
    assert_eq!(
        out,
        vec![
            SummaryLine {
                label: "Dimensions".to_string(),
                value: "1920 x 1080 px".to_string(),
            },
            SummaryLine {
                label: "Kind".to_string(),
                value: "second".to_string(),
            },
        ]
    );
}

#[test]
fn a_paired_line_carries_the_unit_once_at_the_end() {
    let template = Template::parse(TEMPLATE).expect("parses");
    let out = summary(&template, &[1, 0, 2, 0, 1]);
    assert_eq!(out.first().map(|l| l.value.as_str()), Some("1 x 2 px"));
}

#[test]
fn a_template_without_a_summary_has_none_and_falls_back_to_its_name() {
    let template =
        Template::parse("name=\"Plain\"\n[[field]]\nname=\"n\"\ntype=\"u8\"\n").expect("parses");
    assert!(template.summary.is_none());
    assert!(summary(&template, &[1]).is_empty());
    assert_eq!(heading(&template), "Plain");
}

#[test]
fn a_line_naming_a_field_that_is_not_there_is_refused() {
    let bad = TEMPLATE.replace("field = \"width\"", "field = \"widht\"");
    let err = Template::parse(&bad).expect_err("a typo is not a summary");
    assert!(
        err.to_string().contains("no field called \"widht\""),
        "{err}"
    );
    let with = TEMPLATE.replace("with  = \"height\"", "with  = \"heigth\"");
    assert!(Template::parse(&with).is_err());
}

#[test]
fn an_unknown_rendering_is_refused_by_name() {
    let bad = TEMPLATE.replace("render = \"enum\"", "render = \"colour\"");
    let err = Template::parse(&bad).expect_err("no such rendering");
    assert!(err.to_string().contains("is not a rendering"), "{err}");
}

#[test]
fn an_enum_key_that_is_not_a_number_is_refused() {
    let bad = TEMPLATE.replace("{ 1 = \"first\"", "{ one = \"first\"");
    let err = Template::parse(&bad).expect_err("keys are numbers");
    assert!(err.to_string().contains("is not a number"), "{err}");
}

#[test]
fn a_line_whose_field_the_file_does_not_reach_is_left_out() {
    let template = Template::parse(TEMPLATE).expect("parses");
    // Four bytes: the pair renders, the kind is not there at all.
    let out = summary(&template, &[1, 0, 2, 0]);
    assert_eq!(out.len(), 1);
    assert_eq!(out.first().map(|l| l.label.as_str()), Some("Dimensions"));
    assert!(summary(&template, &[]).is_empty());
}

#[test]
fn the_base_key_reads_a_number_written_as_text() {
    let text = r#"
name = "T"
[[field]]
name = "size"
type = "char"
size = 12
[summary]
[[summary.line]]
label  = "Size"
field  = "size"
render = "size"
base   = 8
"#;
    let template = Template::parse(text).expect("parses");
    let out = summary(&template, b"00000000144\0");
    // 0o144 is 100, and it renders as a size rather than as eleven digits.
    assert_eq!(out.first().map(|l| l.value.as_str()), Some("100 B"));
}

// -------------------------------------------------------- picking one

#[test]
fn the_longest_magic_wins_when_more_than_one_matches() {
    let short = Template::parse("name=\"short\"\nmagic={offset=0,bytes=\"AABB\"}\n").expect("a");
    let long = Template::parse("name=\"long\"\nmagic={offset=0,bytes=\"AABBCC\"}\n").expect("b");
    let set = vec![short, long];
    assert_eq!(
        best_match(&set, &[0xAA, 0xBB, 0xCC, 0xDD]).map(|t| t.name.as_str()),
        Some("long")
    );
    // Only the short one can claim these, so it does.
    assert_eq!(
        best_match(&set, &[0xAA, 0xBB, 0x00]).map(|t| t.name.as_str()),
        Some("short")
    );
    assert!(best_match(&set, &[0x00, 0x00, 0x00]).is_none());
    assert!(best_match(&[], &[0xAA]).is_none());
}

#[test]
fn a_tie_keeps_the_first_so_the_answer_does_not_wander() {
    let first = Template::parse("name=\"a\"\nmagic={offset=0,bytes=\"AABB\"}\n").expect("a");
    let second = Template::parse("name=\"b\"\nmagic={offset=0,bytes=\"AABB\"}\n").expect("b");
    let set = vec![first, second];
    assert_eq!(
        best_match(&set, &[0xAA, 0xBB]).map(|t| t.name.as_str()),
        Some("a")
    );
}

#[test]
fn a_rar5_file_is_claimed_by_the_longest_signature_that_fits_it() {
    // The RAR 4 signature is a prefix of the RAR 5 one, which is exactly the
    // case `best_match` exists for. Only RAR 4 is shipped today, so this
    // checks the rule on the shipped set rather than on an invention: an ELF64
    // and an ELF32 magic differ in their last byte, and each must claim only
    // its own.
    let mut elf = vec![0x7F, b'E', b'L', b'F', 2, 1, 1, 0];
    elf.resize(64, 0);
    assert_eq!(
        best_match(builtin(), &elf).map(|t| t.name.as_str()),
        Some("ELF64 header")
    );
    if let Some(byte) = elf.get_mut(4) {
        *byte = 1;
    }
    assert_eq!(
        best_match(builtin(), &elf).map(|t| t.name.as_str()),
        Some("ELF32 header")
    );
}

// -------------------------------------------- the shipped summaries

#[test]
fn every_shipped_summary_names_fields_that_exist() {
    // `Template::parse` refuses a line that names a missing field, so this is
    // really a check that the built-in set parses at all - which is what makes
    // a typo in a summary a failing test rather than a silently missing line.
    assert!(builtin().len() > 100, "{} parsed", builtin().len());
    let with_summary = builtin().iter().filter(|t| t.summary.is_some()).count();
    assert!(with_summary > 60, "only {with_summary} carry a summary");
}

#[test]
fn the_png_summary_reads_a_real_header() {
    let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    png.extend_from_slice(&13_u32.to_be_bytes());
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&1920_u32.to_be_bytes());
    png.extend_from_slice(&1080_u32.to_be_bytes());
    png.extend_from_slice(&[8, 6, 0, 0, 0]);
    png.extend_from_slice(&0_u32.to_be_bytes());

    let out = lines("PNG", &png);
    assert_eq!(value(&out, "Dimensions"), "1920 x 1080 px");
    assert_eq!(value(&out, "Colour"), "RGBA");
    assert_eq!(value(&out, "Bit depth"), "8 bits per channel");
    assert_eq!(value(&out, "Compression"), "deflate");
    assert_eq!(value(&out, "Interlacing"), "none");

    // Colour type 3 is indexed, and a type nobody defined shows as itself.
    let mut indexed = png.clone();
    if let Some(byte) = indexed.get_mut(25) {
        *byte = 3;
    }
    assert_eq!(value(&lines("PNG", &indexed), "Colour"), "indexed");
    if let Some(byte) = indexed.get_mut(25) {
        *byte = 27;
    }
    assert_eq!(value(&lines("PNG", &indexed), "Colour"), "27");
}

#[test]
fn the_gif_and_bmp_summaries_read_real_headers() {
    let mut gif = b"GIF89a".to_vec();
    gif.extend_from_slice(&320_u16.to_le_bytes());
    gif.extend_from_slice(&200_u16.to_le_bytes());
    gif.extend_from_slice(&[0xF7, 0x00, 0x00]);
    let out = lines("GIF header", &gif);
    assert_eq!(value(&out, "Version"), "89a");
    assert_eq!(value(&out, "Dimensions"), "320 x 200 px");
    assert_eq!(value(&out, "Screen"), "global colour table");

    let mut bmp = b"BM".to_vec();
    bmp.extend_from_slice(&1_048_576_u32.to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&54_u32.to_le_bytes());
    bmp.extend_from_slice(&40_u32.to_le_bytes());
    bmp.extend_from_slice(&640_i32.to_le_bytes());
    bmp.extend_from_slice(&480_i32.to_le_bytes());
    bmp.extend_from_slice(&1_u16.to_le_bytes()); // planes, at 26
    bmp.extend_from_slice(&24_u16.to_le_bytes()); // bit count, at 28
    bmp.extend_from_slice(&0_u32.to_le_bytes()); // compression, at 30
    bmp.resize(54, 0);
    let out = lines("BMP", &bmp);
    assert_eq!(value(&out, "Dimensions"), "640 x 480 px");
    assert_eq!(value(&out, "Colour depth"), "24 bit");
    assert_eq!(value(&out, "Compression"), "none");
    assert_eq!(value(&out, "File size"), "1.0 MB");
    assert_eq!(value(&out, "Pixel data at"), "0x36");
}

#[test]
fn the_wav_summary_says_the_sample_rate_the_way_it_is_written_down() {
    let mut wav = b"RIFF".to_vec();
    wav.extend_from_slice(&1_000_000_u32.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&2_u16.to_le_bytes()); // stereo
    wav.extend_from_slice(&44100_u32.to_le_bytes());
    wav.extend_from_slice(&176_400_u32.to_le_bytes());
    wav.extend_from_slice(&4_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&999_956_u32.to_le_bytes());

    let out = lines("WAV (RIFF)", &wav);
    assert_eq!(value(&out, "Format"), "PCM");
    assert_eq!(value(&out, "Channels"), "stereo");
    assert_eq!(value(&out, "Sample rate"), "44.1 kHz");
    assert_eq!(value(&out, "Bit depth"), "16 bit");
    assert_eq!(value(&out, "Data rate"), "176.4 kB/s");
    assert_eq!(value(&out, "Audio data"), "977 KB");

    // Six channels is a number rather than a name, and it still reads as six.
    if let Some(slot) = wav.get_mut(22..24) {
        slot.copy_from_slice(&6_u16.to_le_bytes());
    }
    assert_eq!(value(&lines("WAV (RIFF)", &wav), "Channels"), "6");
}

#[test]
fn the_avi_summary_gives_the_codec_as_its_four_characters() {
    let mut avi = b"RIFF".to_vec();
    avi.extend_from_slice(&100_000_u32.to_le_bytes());
    avi.extend_from_slice(b"AVI LIST");
    avi.extend_from_slice(&192_u32.to_le_bytes());
    avi.extend_from_slice(b"hdrlavih");
    avi.extend_from_slice(&56_u32.to_le_bytes());
    avi.extend_from_slice(&33_366_u32.to_le_bytes()); // microseconds per frame
    avi.extend_from_slice(&1_000_000_u32.to_le_bytes());
    avi.extend_from_slice(&[0; 4]); // padding granularity
    avi.extend_from_slice(&[0; 4]); // flags
    avi.extend_from_slice(&1500_u32.to_le_bytes()); // total frames
    avi.extend_from_slice(&[0; 4]); // initial frames
    avi.extend_from_slice(&1_u32.to_le_bytes()); // streams
    avi.extend_from_slice(&[0; 4]); // suggested buffer size
    avi.extend_from_slice(&1280_u32.to_le_bytes());
    avi.extend_from_slice(&720_u32.to_le_bytes());
    avi.extend_from_slice(&[0; 16]); // reserved, to byte 88
    avi.extend_from_slice(b"LIST");
    avi.extend_from_slice(&120_u32.to_le_bytes());
    avi.extend_from_slice(b"strlstrh");
    avi.extend_from_slice(&56_u32.to_le_bytes());
    avi.extend_from_slice(b"vidsH264");

    let out = lines("AVI (RIFF)", &avi);
    assert_eq!(value(&out, "Dimensions"), "1280 x 720 px");
    assert_eq!(value(&out, "Frames"), "1500");
    assert_eq!(value(&out, "Streams"), "1");
    assert_eq!(value(&out, "Stream type"), "vids");
    assert_eq!(value(&out, "Codec"), "H264");
    assert_eq!(value(&out, "Peak data rate"), "1 MB/s");
}

#[test]
fn the_elf_summary_names_the_machine_and_the_type() {
    let mut elf = vec![0x7F, b'E', b'L', b'F', 2, 1, 1, 3, 0];
    elf.extend_from_slice(&[0; 7]);
    elf.extend_from_slice(&3_u16.to_le_bytes()); // shared object
    elf.extend_from_slice(&0x3E_u16.to_le_bytes()); // x86-64
    elf.extend_from_slice(&1_u32.to_le_bytes());
    elf.extend_from_slice(&0x40_1000_u64.to_le_bytes());
    elf.resize(64, 0);
    if let Some(slot) = elf.get_mut(56..58) {
        slot.copy_from_slice(&9_u16.to_le_bytes()); // e_phnum
    }
    if let Some(slot) = elf.get_mut(60..62) {
        slot.copy_from_slice(&31_u16.to_le_bytes()); // e_shnum
    }

    let out = lines("ELF64 header", &elf);
    assert_eq!(value(&out, "Class"), "64-bit");
    assert_eq!(value(&out, "Byte order"), "little endian");
    assert_eq!(value(&out, "OS/ABI"), "Linux");
    assert_eq!(value(&out, "Type"), "shared object");
    assert_eq!(value(&out, "Machine"), "x86-64");
    assert_eq!(value(&out, "Entry point"), "0x401000");
    assert_eq!(value(&out, "Segments"), "9");
    assert_eq!(value(&out, "Sections"), "31");

    // AArch64, which is the other machine anyone reads this for.
    if let Some(slot) = elf.get_mut(18..20) {
        slot.copy_from_slice(&183_u16.to_le_bytes());
    }
    assert_eq!(value(&lines("ELF64 header", &elf), "Machine"), "AArch64");
}

#[test]
fn the_pe_summary_names_the_machine_the_subsystem_and_the_flags() {
    let mut pe = b"PE\0\0".to_vec();
    pe.extend_from_slice(&0x8664_u16.to_le_bytes()); // x86-64
    pe.extend_from_slice(&6_u16.to_le_bytes()); // sections
    pe.extend_from_slice(&1_700_000_000_u32.to_le_bytes());
    pe.extend_from_slice(&[0; 8]);
    pe.extend_from_slice(&240_u16.to_le_bytes()); // optional header size
    pe.extend_from_slice(&0x2022_u16.to_le_bytes()); // executable, large address aware, DLL
    pe.extend_from_slice(&0x20B_u16.to_le_bytes()); // PE32+
    pe.resize(136, 0);
    if let Some(slot) = pe.get_mut(40..44) {
        slot.copy_from_slice(&0x1500_u32.to_le_bytes()); // entry point
    }
    if let Some(slot) = pe.get_mut(48..56) {
        slot.copy_from_slice(&0x1_4000_0000_u64.to_le_bytes()); // image base
    }
    if let Some(slot) = pe.get_mut(80..84) {
        slot.copy_from_slice(&2_097_152_u32.to_le_bytes()); // image size
    }
    if let Some(slot) = pe.get_mut(92..94) {
        slot.copy_from_slice(&3_u16.to_le_bytes()); // console
    }
    if let Some(slot) = pe.get_mut(94..96) {
        slot.copy_from_slice(&0xC160_u16.to_le_bytes()); // ASLR, DEP, CFG, TS aware
    }

    let template = shipped("PE header (PE32+)");
    let out: Vec<(String, String)> = summary(template, &pe)
        .into_iter()
        .map(|l| (l.label, l.value))
        .collect();
    assert_eq!(value(&out, "Machine"), "x86-64");
    assert_eq!(value(&out, "Format"), "PE32+");
    assert_eq!(value(&out, "Subsystem"), "Windows console");
    assert_eq!(
        value(&out, "Attributes"),
        "executable, large address aware, DLL"
    );
    assert_eq!(
        value(&out, "Mitigations"),
        "ASLR, DEP, control flow guard, terminal server aware"
    );
    assert_eq!(value(&out, "Sections"), "6");
    assert_eq!(value(&out, "Entry point"), "0x1500");
    assert_eq!(value(&out, "Image base"), "0x140000000");
    assert_eq!(value(&out, "Image size"), "2.0 MB");
    assert_eq!(value(&out, "Built"), "2023-11-14 22:13:20");
}

#[test]
fn the_zip_and_gzip_summaries_name_the_compression() {
    let mut zip = vec![0x50, 0x4B, 0x03, 0x04];
    zip.extend_from_slice(&20_u16.to_le_bytes());
    zip.extend_from_slice(&0x0808_u16.to_le_bytes()); // sizes follow, UTF-8 name
    zip.extend_from_slice(&8_u16.to_le_bytes()); // deflate
    zip.extend_from_slice(&[0; 4]);
    zip.extend_from_slice(&0_u32.to_le_bytes());
    zip.extend_from_slice(&4096_u32.to_le_bytes());
    zip.extend_from_slice(&16384_u32.to_le_bytes());
    zip.extend_from_slice(&5_u16.to_le_bytes());
    zip.extend_from_slice(&0_u16.to_le_bytes());
    let out = lines("ZIP local file header", &zip);
    assert_eq!(value(&out, "Compression"), "deflate");
    assert_eq!(value(&out, "Compressed"), "4.0 KB");
    assert_eq!(value(&out, "Uncompressed"), "16 KB");
    assert_eq!(value(&out, "Flags"), "sizes follow the data, UTF-8 name");

    let mut gzip = vec![0x1F, 0x8B, 0x08, 0x08];
    gzip.extend_from_slice(&1_700_000_000_u32.to_le_bytes());
    gzip.extend_from_slice(&[0x00, 0x03]);
    let out = lines("GZIP", &gzip);
    assert_eq!(value(&out, "Compression"), "deflate");
    assert_eq!(value(&out, "Compressed on"), "Unix");
    assert_eq!(value(&out, "Original modified"), "2023-11-14 22:13:20");
    assert_eq!(value(&out, "Header"), "original name");
}

#[test]
fn the_tar_summary_reads_the_octal_fields_as_numbers() {
    let mut block = vec![0_u8; 512];
    block[..5].copy_from_slice(b"a.txt");
    block[100..107].copy_from_slice(b"0000644");
    block[124..135].copy_from_slice(b"00000000144");
    block[136..147].copy_from_slice(b"14012303514");
    block[156] = b'0';
    block[257..263].copy_from_slice(b"ustar\0");
    block[265..269].copy_from_slice(b"root");
    block[297..302].copy_from_slice(b"wheel");

    let out = lines("TAR (ustar)", &block);
    assert_eq!(value(&out, "First member"), "a.txt");
    assert_eq!(value(&out, "Its size"), "100 B");
    assert_eq!(value(&out, "Its type"), "regular file");
    assert_eq!(value(&out, "Its mode"), "0000644");
    assert_eq!(value(&out, "Owner"), "root/wheel");
    // 0o14012303514 is 1613334348, a real second in 2021.
    assert_eq!(value(&out, "Modified"), "2021-02-14 20:25:48");

    // A directory member, and a pax extended header, whose type letter is not
    // a number and shows as itself.
    block[156] = b'5';
    assert_eq!(
        value(&lines("TAR (ustar)", &block), "Its type"),
        "directory"
    );
    block[156] = b'x';
    assert_eq!(value(&lines("TAR (ustar)", &block), "Its type"), "\"x\"");
}

#[test]
fn the_sqlite_summary_names_the_encoding() {
    let mut db = b"SQLite format 3\0".to_vec();
    db.extend_from_slice(&4096_u16.to_be_bytes());
    db.resize(100, 0);
    if let Some(slot) = db.get_mut(18..20) {
        slot.copy_from_slice(&[2, 2]); // write and read version: WAL
    }
    if let Some(slot) = db.get_mut(28..32) {
        slot.copy_from_slice(&512_u32.to_be_bytes()); // pages
    }
    if let Some(slot) = db.get_mut(56..60) {
        slot.copy_from_slice(&1_u32.to_be_bytes()); // UTF-8
    }
    if let Some(slot) = db.get_mut(96..100) {
        slot.copy_from_slice(&3_045_000_u32.to_be_bytes());
    }
    let out = lines("SQLite 3", &db);
    assert_eq!(value(&out, "Page size"), "4096 bytes");
    assert_eq!(value(&out, "Pages"), "512");
    assert_eq!(value(&out, "Text encoding"), "UTF-8");
    assert_eq!(value(&out, "Write mode"), "write-ahead log");
    assert_eq!(value(&out, "Written by SQLite"), "3045000");
}

#[test]
fn the_java_summary_turns_a_class_version_into_a_java_release() {
    let mut class = vec![0xCA, 0xFE, 0xBA, 0xBE];
    class.extend_from_slice(&0_u16.to_be_bytes());
    class.extend_from_slice(&65_u16.to_be_bytes());
    class.extend_from_slice(&180_u16.to_be_bytes());
    let out = lines("Java class", &class);
    assert_eq!(value(&out, "Compiled for"), "Java 21");
    assert_eq!(value(&out, "Class file version"), "65.0");
    assert_eq!(value(&out, "Constant pool"), "180 entries");

    // A release nobody has named yet shows the number rather than a guess.
    if let Some(slot) = class.get_mut(6..8) {
        slot.copy_from_slice(&99_u16.to_be_bytes());
    }
    assert_eq!(value(&lines("Java class", &class), "Compiled for"), "99");
}

#[test]
fn the_pyc_summary_names_the_python_that_compiled_it() {
    let mut pyc = 3531_u16.to_le_bytes().to_vec(); // 3.12
    pyc.extend_from_slice(&[0x0D, 0x0A]);
    pyc.extend_from_slice(&0_u32.to_le_bytes());
    pyc.extend_from_slice(&1_700_000_000_u32.to_le_bytes());
    pyc.extend_from_slice(&2048_u32.to_le_bytes());
    let out = lines("Python pyc (3.7+)", &pyc);
    assert_eq!(value(&out, "Compiled by"), "Python 3.12");
    assert_eq!(value(&out, "Source last changed"), "2023-11-14 22:13:20");
    assert_eq!(value(&out, "Source size"), "2.0 KB");
    assert_eq!(value(&out, "Invalidation"), "none");
}

#[test]
fn the_dex_summary_counts_what_is_in_the_file() {
    let mut dex = b"dex\n035\0".to_vec();
    dex.resize(112, 0);
    if let Some(slot) = dex.get_mut(32..36) {
        slot.copy_from_slice(&1_048_576_u32.to_le_bytes());
    }
    if let Some(slot) = dex.get_mut(56..60) {
        slot.copy_from_slice(&4000_u32.to_le_bytes()); // strings
    }
    if let Some(slot) = dex.get_mut(88..92) {
        slot.copy_from_slice(&2500_u32.to_le_bytes()); // methods
    }
    if let Some(slot) = dex.get_mut(96..100) {
        slot.copy_from_slice(&300_u32.to_le_bytes()); // classes
    }
    let out = lines("Android DEX", &dex);
    assert_eq!(value(&out, "Version"), "dex.035");
    assert_eq!(value(&out, "File size"), "1.0 MB");
    assert_eq!(value(&out, "Classes"), "300");
    assert_eq!(value(&out, "Methods"), "2500");
    assert_eq!(value(&out, "Strings"), "4000");
}

#[test]
fn the_partition_summaries_name_the_types() {
    let mut disk = vec![0_u8; 1024];
    disk[446] = 0x80;
    disk[450] = 0xEE; // GPT protective
    disk[454..458].copy_from_slice(&1_u32.to_le_bytes());
    disk[458..462].copy_from_slice(&2_097_152_u32.to_le_bytes());
    disk[466] = 0x83; // Linux
    disk[474..478].copy_from_slice(&20480_u32.to_le_bytes());
    disk[510..512].copy_from_slice(&[0x55, 0xAA]);
    let out = lines("MBR", &disk);
    assert_eq!(value(&out, "Partition 1"), "GPT protective");
    assert_eq!(value(&out, "Partition 1 size"), "1.0 GB");
    assert_eq!(value(&out, "Partition 2"), "Linux");
    assert_eq!(value(&out, "Partition 2 size"), "10 MB");
    assert_eq!(value(&out, "Partition 3"), "empty");

    disk[512..520].copy_from_slice(b"EFI PART");
    disk[520..524].copy_from_slice(&0x0001_0000_u32.to_le_bytes());
    disk[524..528].copy_from_slice(&92_u32.to_le_bytes());
    disk[584..592].copy_from_slice(&2_u64.to_le_bytes());
    disk[592..596].copy_from_slice(&128_u32.to_le_bytes());
    disk[596..600].copy_from_slice(&128_u32.to_le_bytes());
    let out = lines("GPT header", &disk);
    assert_eq!(value(&out, "Revision"), "0x10000");
    assert_eq!(value(&out, "Header size"), "92 bytes");
    assert_eq!(value(&out, "Entries"), "128");
    assert_eq!(value(&out, "Entry array at block"), "2");
}

#[test]
fn the_filesystem_summaries_read_real_superblocks() {
    let mut volume = vec![0_u8; 512];
    volume[..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
    volume[3..11].copy_from_slice(b"mkfs.fat");
    volume[11..13].copy_from_slice(&512_u16.to_le_bytes());
    volume[13] = 8;
    volume[16] = 2;
    volume[32..36].copy_from_slice(&2_097_152_u32.to_le_bytes());
    volume[36..40].copy_from_slice(&2048_u32.to_le_bytes());
    volume[44..48].copy_from_slice(&2_u32.to_le_bytes());
    volume[67..71].copy_from_slice(&0xDEAD_BEEF_u32.to_le_bytes());
    volume[71..82].copy_from_slice(b"BACKUP     ");
    volume[82..90].copy_from_slice(b"FAT32   ");
    volume[510..512].copy_from_slice(&[0x55, 0xAA]);
    let out = lines("FAT32 BPB", &volume);
    assert_eq!(value(&out, "Volume label"), "BACKUP");
    assert_eq!(value(&out, "Declared type"), "FAT32");
    assert_eq!(value(&out, "Bytes per sector"), "512");
    assert_eq!(value(&out, "Sectors per cluster"), "8");
    assert_eq!(value(&out, "Total sectors"), "2097152");
    assert_eq!(value(&out, "Serial number"), "0xDEADBEEF");

    let mut volume = vec![0_u8; 2048];
    volume[1024..1028].copy_from_slice(&65_536_u32.to_le_bytes());
    volume[1024 + 4..1024 + 8].copy_from_slice(&262_144_u32.to_le_bytes());
    volume[1024 + 12..1024 + 16].copy_from_slice(&100_000_u32.to_le_bytes());
    volume[1024 + 24..1024 + 28].copy_from_slice(&2_u32.to_le_bytes());
    volume[1024 + 48..1024 + 52].copy_from_slice(&1_700_000_000_u32.to_le_bytes());
    volume[1024 + 56..1024 + 58].copy_from_slice(&0xEF53_u16.to_le_bytes());
    volume[1024 + 58..1024 + 60].copy_from_slice(&1_u16.to_le_bytes());
    volume[1024 + 60..1024 + 62].copy_from_slice(&1_u16.to_le_bytes());
    volume[1024 + 88..1024 + 90].copy_from_slice(&256_u16.to_le_bytes());
    volume[1024 + 120..1024 + 126].copy_from_slice(b"backup");
    volume[1024 + 136..1024 + 141].copy_from_slice(b"/data");
    let out = lines("ext2/3/4 superblock", &volume);
    assert_eq!(value(&out, "Volume name"), "backup");
    assert_eq!(value(&out, "Last mounted on"), "/data");
    assert_eq!(value(&out, "Block size"), "4 KB");
    assert_eq!(value(&out, "Blocks"), "262144");
    assert_eq!(value(&out, "Inodes"), "65536");
    assert_eq!(value(&out, "Inode size"), "256 bytes");
    assert_eq!(value(&out, "State"), "cleanly unmounted");
    assert_eq!(value(&out, "On error"), "continue");
    assert_eq!(value(&out, "Last written"), "2023-11-14 22:13:20");
}

#[test]
fn the_iso_summary_reads_the_volume_identifiers() {
    let mut image = vec![0_u8; 32768 + 2048];
    image[32768] = 1;
    image[32769..32774].copy_from_slice(b"CD001");
    image[32775] = 1;
    image[32768 + 8..32768 + 13].copy_from_slice(b"LINUX");
    image[32768 + 40..32768 + 48].copy_from_slice(b"DEBIAN12");
    image[32768 + 80..32768 + 84].copy_from_slice(&256_000_u32.to_le_bytes());
    image[32768 + 128..32768 + 130].copy_from_slice(&2048_u16.to_le_bytes());
    image[32768 + 318..32768 + 324].copy_from_slice(b"DEBIAN");
    let out = lines("ISO 9660 PVD", &image);
    assert_eq!(value(&out, "Volume"), "DEBIAN12");
    assert_eq!(value(&out, "System"), "LINUX");
    assert_eq!(value(&out, "Publisher"), "DEBIAN");
    // 256000 blocks of 2048 bytes.
    assert_eq!(value(&out, "Size"), "500 MB");
    assert_eq!(value(&out, "Block size"), "2048 bytes");
}
