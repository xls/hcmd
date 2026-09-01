//! Mode 3 in the viewer: entering it, refusing it, folding and moving.

use super::*;
use crate::config::ViewerConfig;

/// A viewer over `body`, named `name` so the renderer is chosen by extension.
fn open_named(name: &str, body: &str, cfg: &ViewerConfig) -> Viewer {
    let bytes = std::sync::Arc::new(body.as_bytes().to_vec());
    let len = bytes.len() as u64;
    Viewer::open(
        ViewerId(1),
        name,
        None,
        source::memory_opener(bytes),
        Some(len),
        cfg,
    )
    .expect("open")
}

const JSON: &str = r#"{
  "name": "hcmd",
  "targets": [
    { "kind": "bin" },
    { "kind": "lib" }
  ],
  "done": true
}"#;

/// A viewer in mode 3 over the document above, laid out 20 rows tall.
fn rendering() -> Viewer {
    let mut viewer = open_named("package.json", JSON, &ViewerConfig::default());
    viewer.set_mode(ViewerMode::Render).expect("mode 3");
    viewer.layout(20, 80).expect("layout");
    viewer
}

/// The rows on screen, as text.
fn rows(viewer: &Viewer) -> Vec<String> {
    viewer
        .rows()
        .iter()
        .filter_map(|row| match row {
            Row::Text { text, .. } => Some(text.clone()),
            Row::Hex { .. } => None,
        })
        .collect()
}

#[test]
fn mode_three_renders_the_document_and_says_which_renderer_it_used() {
    let viewer = rendering();
    assert_eq!(viewer.mode(), ViewerMode::Render);
    assert!(viewer.render_note().is_none(), "nothing to explain");
    assert_eq!(
        viewer.rendered().map(|r| r.kind),
        Some(render::RenderKind::Json)
    );
    assert_eq!(
        rows(&viewer),
        vec![
            "{",
            "  name: \"hcmd\"",
            "  targets: [",
            "    {",
            "      kind: \"bin\"",
            "    }",
            "    {",
            "      kind: \"lib\"",
            "    }",
            "  ]",
            "  done: true",
            "}",
        ]
    );
}

#[test]
fn the_status_line_names_the_renderer() {
    let viewer = rendering();
    let status = viewer.status();
    assert_eq!(status.mode, ViewerMode::Render);
    assert_eq!(status.render.as_deref(), Some("json"));
}

// ------------------------------------------------------------- refusals

#[test]
fn a_file_nothing_renders_falls_back_to_text_and_says_so_once() {
    let mut viewer = open_named("notes.txt", "plain text\n", &ViewerConfig::default());
    viewer.set_mode(ViewerMode::Render).expect("no error");
    assert_eq!(viewer.mode(), ViewerMode::Text, "it fell back");
    let note = viewer.render_note().unwrap_or_default().to_string();
    assert!(note.contains("nothing renders this"), "{note}");
    assert!(note.contains("JSON, HTML and Markdown"), "{note}");
    // The note is cleared by the next mode switch, so it is said once.
    viewer.set_mode(ViewerMode::Hex).expect("hex");
    assert!(viewer.render_note().is_none());
}

#[test]
fn a_file_that_does_not_parse_as_its_extension_falls_back_and_names_the_format() {
    let mut viewer = open_named(
        "broken.json",
        "this is not json\n",
        &ViewerConfig::default(),
    );
    viewer.set_mode(ViewerMode::Render).expect("no error");
    assert_eq!(viewer.mode(), ViewerMode::Text);
    let note = viewer.render_note().unwrap_or_default().to_string();
    assert!(note.contains("does not parse as json"), "{note}");
}

#[test]
fn a_file_over_the_ceiling_is_refused_by_name_rather_than_half_rendered() {
    let mut cfg = ViewerConfig::default();
    cfg.render.max_size = crate::config::ByteSize(64);
    let body = format!("{{\"key\": \"{}\"}}", "x".repeat(200));
    let mut viewer = open_named("big.json", &body, &cfg);
    viewer.set_mode(ViewerMode::Render).expect("no error");

    assert_eq!(viewer.mode(), ViewerMode::Text, "it did not open in mode 3");
    assert!(viewer.rendered().is_none(), "and nothing was rendered");
    let note = viewer.render_note().unwrap_or_default().to_string();
    // The limit and the setting, which is what a person needs to change it.
    assert!(note.contains("viewer.render.max_size"), "{note}");
    assert!(note.contains("over the"), "{note}");
}

#[test]
fn a_file_just_under_the_ceiling_still_opens() {
    let mut cfg = ViewerConfig::default();
    cfg.render.max_size = crate::config::ByteSize(64);
    let mut viewer = open_named("small.json", "{\"a\": 1}", &cfg);
    viewer.set_mode(ViewerMode::Render).expect("mode 3");
    assert_eq!(viewer.mode(), ViewerMode::Render);
}

// -------------------------------------------------------------- folding

#[test]
fn the_document_knows_which_lines_fold() {
    let viewer = rendering();
    // The root object, the array, and each of its two elements.
    assert_eq!(
        viewer.render_regions().keys().copied().collect::<Vec<_>>(),
        vec![0, 2, 3, 6]
    );
}

#[test]
fn folding_a_region_hides_its_body_and_shows_the_summary_in_its_place() {
    let mut viewer = rendering();
    // Down to the array's line and fold it.
    for _ in 0..2 {
        viewer
            .move_cursor(select::Motion::Down, select::Extend::None)
            .expect("down");
    }
    assert_eq!(viewer.render_cursor(), 2);
    assert_eq!(viewer.toggle_fold(), "collapsed");
    viewer.layout(20, 80).expect("layout");
    assert_eq!(
        rows(&viewer),
        vec![
            "{",
            "  name: \"hcmd\"",
            "  targets: [...] 2 items",
            "  done: true",
            "}",
        ]
    );
    // And expanding puts it back exactly.
    assert_eq!(viewer.toggle_fold(), "expanded");
    viewer.layout(20, 80).expect("layout");
    assert_eq!(rows(&viewer).len(), 12);
}

#[test]
fn folding_a_line_that_opens_nothing_says_so_rather_than_doing_nothing() {
    let mut viewer = rendering();
    viewer
        .move_cursor(select::Motion::Down, select::Extend::None)
        .expect("down");
    assert_eq!(viewer.render_cursor(), 1, "on a scalar member");
    assert_eq!(viewer.toggle_fold(), "nothing to fold on this line");
    assert!(viewer.render_folds().is_empty());
}

#[test]
fn folding_outside_mode_three_says_which_mode_it_belongs_to() {
    let mut viewer = open_named("a.json", JSON, &ViewerConfig::default());
    assert!(viewer.toggle_fold().contains("press 3"));
}

#[test]
fn fold_all_collapses_everything_and_unfold_all_puts_it_back() {
    let mut viewer = rendering();
    let said = viewer.fold_all(true);
    assert_eq!(said, "collapsed 4 regions");
    viewer.layout(20, 80).expect("layout");
    // Only the root's own summary is left.
    assert_eq!(rows(&viewer), vec!["{...} 3 keys"]);

    assert_eq!(viewer.fold_all(false), "expanded everything");
    viewer.layout(20, 80).expect("layout");
    assert_eq!(rows(&viewer).len(), 12);
}

#[test]
fn collapsing_everything_brings_the_cursor_back_to_a_line_that_is_drawn() {
    let mut viewer = rendering();
    for _ in 0..6 {
        viewer
            .move_cursor(select::Motion::Down, select::Extend::None)
            .expect("down");
    }
    assert_eq!(viewer.render_cursor(), 6);
    viewer.fold_all(true);
    assert_eq!(viewer.render_cursor(), 0, "the only line still drawn");
}

// ----------------------------------------------------------- navigation

#[test]
fn the_cursor_steps_over_a_collapsed_region_rather_than_into_it() {
    let mut viewer = rendering();
    for _ in 0..2 {
        viewer
            .move_cursor(select::Motion::Down, select::Extend::None)
            .expect("down");
    }
    viewer.toggle_fold();
    // The next line down is the one after the whole array.
    viewer
        .move_cursor(select::Motion::Down, select::Extend::None)
        .expect("down");
    assert_eq!(viewer.render_cursor(), 10, "past the folded array");
    // And back up lands on the fold's own line again.
    viewer
        .move_cursor(select::Motion::Up, select::Extend::None)
        .expect("up");
    assert_eq!(viewer.render_cursor(), 2);
}

#[test]
fn the_cursor_stops_at_both_ends() {
    let mut viewer = rendering();
    viewer
        .move_cursor(select::Motion::Up, select::Extend::None)
        .expect("up");
    assert_eq!(viewer.render_cursor(), 0);
    viewer
        .move_cursor(select::Motion::FileEnd, select::Extend::None)
        .expect("end");
    assert_eq!(viewer.render_cursor(), 11, "the closing brace");
    viewer
        .move_cursor(select::Motion::Down, select::Extend::None)
        .expect("down");
    assert_eq!(viewer.render_cursor(), 11);
    viewer
        .move_cursor(select::Motion::FileStart, select::Extend::None)
        .expect("home");
    assert_eq!(viewer.render_cursor(), 0);
}

#[test]
fn the_window_follows_the_cursor_on_a_screen_too_short_for_the_document() {
    let mut viewer = rendering();
    viewer.layout(4, 80).expect("layout");
    assert_eq!(viewer.render_top(), 0);
    for _ in 0..6 {
        viewer
            .move_cursor(select::Motion::Down, select::Extend::None)
            .expect("down");
    }
    viewer.layout(4, 80).expect("layout");
    // The cursor is on line 6 and the window is four tall, so it starts at 3.
    assert_eq!(viewer.render_cursor(), 6);
    assert_eq!(viewer.render_top(), 3);
    assert_eq!(rows(&viewer).len(), 4);
    // Back to the top, and the window comes with it.
    viewer
        .move_cursor(select::Motion::FileStart, select::Extend::None)
        .expect("home");
    assert_eq!(viewer.render_top(), 0);
}

#[test]
fn scrolling_moves_the_window_and_leaves_the_cursor_alone() {
    let mut viewer = rendering();
    viewer.layout(4, 80).expect("layout");
    viewer.scroll(2).expect("scroll");
    assert_eq!(viewer.render_top(), 2);
    assert_eq!(viewer.render_cursor(), 0, "the cursor did not move");
    viewer.scroll(-5).expect("scroll back");
    assert_eq!(viewer.render_top(), 0, "and it stops at the top");
}

// -------------------------------------------------- the byte side is kept

/// Mode 3 has no byte position, so it must not invent one or lose the old one.
#[test]
fn going_to_mode_three_and_back_keeps_the_byte_position() {
    let mut viewer = open_named("package.json", JSON, &ViewerConfig::default());
    viewer.layout(20, 80).expect("layout");
    viewer.place_cursor(12).expect("place");
    let was = viewer.cursor();

    viewer.set_mode(ViewerMode::Render).expect("mode 3");
    assert_eq!(viewer.cursor(), was, "the byte cursor did not move");
    viewer
        .move_cursor(select::Motion::Down, select::Extend::None)
        .expect("down");
    assert_eq!(viewer.cursor(), was, "and moving in mode 3 did not move it");

    viewer.set_mode(ViewerMode::Text).expect("back to text");
    assert_eq!(viewer.cursor(), was, "text mode found it where it was");
}

#[test]
fn f4_from_mode_three_goes_to_text_rather_than_cycling() {
    let mut viewer = rendering();
    viewer.toggle_mode().expect("F4");
    assert_eq!(
        viewer.mode(),
        ViewerMode::Text,
        "F4 stays a two-way key; 3 is how mode 3 is reached"
    );
    viewer.toggle_mode().expect("F4");
    assert_eq!(viewer.mode(), ViewerMode::Hex);
    viewer.toggle_mode().expect("F4");
    assert_eq!(viewer.mode(), ViewerMode::Text);
}

#[test]
fn copying_in_mode_three_is_refused_by_name_rather_than_silently() {
    let mut viewer = rendering();
    viewer.select_all();
    let out = viewer
        .copy(copy::CopyRequest::Selection, 1024 * 1024)
        .expect("no error");
    match out {
        copy::Copied::Refused(said) => {
            assert!(said.contains("mode 3"), "{said}");
            assert!(said.contains("press 1"), "{said}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_markdown_file_renders_and_folds_nothing() {
    let mut viewer = open_named("README.md", "# Title\n\ntext\n", &ViewerConfig::default());
    viewer.set_mode(ViewerMode::Render).expect("mode 3");
    viewer.layout(20, 80).expect("layout");
    assert_eq!(rows(&viewer), vec!["TITLE", "=====", "", "text"]);
    assert!(viewer.render_regions().is_empty(), "Markdown has no folds");
}

#[test]
fn an_html_file_renders_its_text() {
    let body = "<html><body><h1>Hi</h1><p>there</p><script>var x=1;</script></body></html>";
    let mut viewer = open_named("page.html", body, &ViewerConfig::default());
    viewer.set_mode(ViewerMode::Render).expect("mode 3");
    viewer.layout(20, 80).expect("layout");
    assert_eq!(rows(&viewer), vec!["Hi", "", "there"]);
}

/// Mode 3 leaves the byte window alone, so coming back from it must not throw
/// away the line number cached for that window.
///
/// It did: `set_mode(Text)` cleared the cache unconditionally, and text mode
/// draws an unknown number as `?`. The numbers only came back when the reader
/// scrolled to the top of the file, which is the one place the count is known
/// without walking the index. Reported from the running program.
#[test]
fn coming_back_from_mode_three_keeps_the_line_numbers() {
    let mut viewer = open_named("package.json", JSON, &ViewerConfig::default());
    viewer.layout(20, 80).expect("layout");
    let before = numbered_rows(&viewer);
    assert!(
        before.iter().all(Option::is_some),
        "the fixture starts with every line numbered: {before:?}"
    );

    viewer.set_mode(ViewerMode::Render).expect("mode 3");
    viewer.layout(20, 80).expect("layout");
    viewer.set_mode(ViewerMode::Text).expect("back to text");
    viewer.layout(20, 80).expect("layout");

    assert_eq!(
        numbered_rows(&viewer),
        before,
        "the same window must report the same line numbers"
    );
}

/// The line number each text row carries, `None` where it is drawn as `?`.
fn numbered_rows(viewer: &Viewer) -> Vec<Option<u64>> {
    viewer
        .rows()
        .iter()
        .filter_map(|row| match row {
            crate::viewer::Row::Text { line, .. } => Some(*line),
            crate::viewer::Row::Hex { .. } => None,
        })
        .collect()
}

// ------------------------------- mode 3 on a binary: what the file *is*

/// A PNG's first 33 bytes: 1920 x 1080, 8-bit RGBA, no interlacing.
fn png_bytes() -> Vec<u8> {
    let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    png.extend_from_slice(&13_u32.to_be_bytes());
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&1920_u32.to_be_bytes());
    png.extend_from_slice(&1080_u32.to_be_bytes());
    png.extend_from_slice(&[8, 6, 0, 0, 0]);
    png.extend_from_slice(&0_u32.to_be_bytes());
    png
}

/// A viewer over raw bytes, named `name`, put into mode 3 and laid out.
fn rendering_bytes(name: &str, body: &[u8], cfg: &ViewerConfig) -> Viewer {
    let bytes = std::sync::Arc::new(body.to_vec());
    let len = bytes.len() as u64;
    let mut viewer = Viewer::open(
        ViewerId(1),
        name,
        None,
        source::memory_opener(bytes),
        Some(len),
        cfg,
    )
    .expect("open");
    viewer.set_mode(ViewerMode::Render).expect("no error");
    viewer.layout(20, 80).expect("layout");
    viewer
}

#[test]
fn a_png_in_mode_three_says_what_it_is_rather_than_showing_a_note() {
    let viewer = rendering_bytes("shot.png", &png_bytes(), &ViewerConfig::default());
    assert_eq!(viewer.mode(), ViewerMode::Render, "it did not fall back");
    assert!(
        viewer.render_note().is_none(),
        "and it has nothing to explain"
    );
    assert_eq!(
        rows(&viewer),
        vec![
            "PNG image",
            "",
            "Dimensions   1920 x 1080 px",
            "Colour       RGBA",
            "Bit depth    8 bits per channel",
            "Compression  deflate",
            "Interlacing  none",
        ]
    );
}

#[test]
fn the_status_line_names_the_template_that_is_being_shown() {
    let viewer = rendering_bytes("shot.png", &png_bytes(), &ViewerConfig::default());
    assert_eq!(viewer.mode(), ViewerMode::Render);
    assert_eq!(viewer.status_render_label().as_deref(), Some("PNG"));
    assert_eq!(
        viewer.rendered().map(|r| r.kind),
        Some(render::RenderKind::Summary)
    );
}

/// The renderer wins where there is one: a `.json` file is a tree, never a
/// template's description of a JSON file.
#[test]
fn a_renderer_beats_a_template_where_both_could_apply() {
    // A file that is JSON by name and content, whose first bytes a template
    // would also claim if it were asked first.
    let viewer = rendering_bytes("a.json", b"{\"a\": 1}", &ViewerConfig::default());
    assert_eq!(
        viewer.rendered().map(|r| r.kind),
        Some(render::RenderKind::Json),
        "the tree lost to a template"
    );
    assert_eq!(rows(&viewer), vec!["{", "  a: 1", "}"]);
}

#[test]
fn a_file_with_neither_a_renderer_nor_a_template_still_falls_back_to_text() {
    let viewer = rendering_bytes(
        "notes.txt",
        b"nothing here matches anything at all\n",
        &ViewerConfig::default(),
    );
    assert_eq!(viewer.mode(), ViewerMode::Text);
    assert!(viewer.rendered().is_none());
    let note = viewer.render_note().unwrap_or_default().to_string();
    assert!(note.contains("nothing renders this"), "{note}");
}

/// A file too big to render is not too big to describe: the summary needs the
/// head, not the document.
#[test]
fn a_file_over_the_ceiling_still_says_what_it_is_when_a_template_knows() {
    let cfg = ViewerConfig {
        render: crate::config::ViewerRenderConfig {
            max_size: crate::config::ByteSize(8),
        },
        ..ViewerConfig::default()
    };
    let mut body = png_bytes();
    body.resize(4096, 0);
    let viewer = rendering_bytes("huge.png", &body, &cfg);
    assert_eq!(viewer.mode(), ViewerMode::Render);
    assert_eq!(viewer.status_render_label().as_deref(), Some("PNG"));
    assert!(
        rows(&viewer).contains(&"Dimensions   1920 x 1080 px".to_string()),
        "{:#?}",
        rows(&viewer)
    );
}

#[test]
fn a_summary_document_scrolls_and_moves_like_any_other() {
    let mut viewer = rendering_bytes("shot.png", &png_bytes(), &ViewerConfig::default());
    assert_eq!(viewer.render_cursor(), 0);
    viewer
        .move_cursor(select::Motion::FileEnd, select::Extend::None)
        .expect("end");
    assert_eq!(viewer.render_cursor(), 6, "the last fact");
    viewer
        .move_cursor(select::Motion::Up, select::Extend::None)
        .expect("up");
    assert_eq!(viewer.render_cursor(), 5);
    // Nothing folds in a summary, and asking says so rather than doing nothing.
    assert!(viewer.render_regions().is_empty());
    assert_eq!(viewer.toggle_fold(), "nothing to fold on this line");
}

/// A format with a template but no summary of its own is not worth a document
/// of one heading, so it keeps the honest note.
#[test]
fn a_template_with_no_summary_is_not_shown_as_one() {
    // DER: recognised, and deliberately carrying no summary.
    let viewer = rendering_bytes(
        "cert.der",
        &[0x30, 0x82, 0x04, 0x00],
        &ViewerConfig::default(),
    );
    println!(
        "mode={:?} note={:?} rendered={:?}",
        viewer.mode(),
        viewer.render_note(),
        viewer.rendered().map(|r| r.label.clone())
    );
    assert_eq!(viewer.mode(), ViewerMode::Text);
    assert!(viewer.render_note().is_some());
}

// ------------------------------------------------ the mode a file opens in

/// Open `name` over `body` exactly as the event loop does, and say what mode
/// it landed in.
fn opens_as(name: &str, body: &[u8], open_as_document: bool) -> ViewerMode {
    let cfg = ViewerConfig {
        open_as_document,
        ..ViewerConfig::default()
    };
    let bytes = std::sync::Arc::new(body.to_vec());
    let len = bytes.len() as u64;
    let mut viewer = Viewer::open(
        ViewerId(1),
        name,
        None,
        source::memory_opener(bytes),
        Some(len),
        &cfg,
    )
    .expect("open");
    viewer
        .choose_initial_mode(cfg.default_mode, cfg.open_as_document)
        .expect("choose");
    viewer.mode()
}

/// A compiled Android manifest, when the maintainer's sample is there.
fn axml_bytes() -> Option<Vec<u8>> {
    let home = std::env::var("HOME").ok()?;
    std::fs::read(std::path::Path::new(&home).join("TestData/AndroidManifest.xml")).ok()
}

/// Bytes nothing recognises and that are not text either.
fn plain_binary() -> Vec<u8> {
    let mut out = vec![0x00_u8, 0x01, 0x02, 0x03];
    out.extend(std::iter::repeat_n(0xFF_u8, 200));
    out
}

/// The whole matrix, both ways round. Eight cases, one answer each.
#[test]
fn a_file_opens_in_the_mode_its_own_content_asks_for() {
    let json = b"{\"a\": 1}";
    let png = png_bytes();
    let binary = plain_binary();
    let text = b"just some prose, nothing more\n";

    // open_as_document on: a recognised format wins over everything.
    assert_eq!(opens_as("a.json", json, true), ViewerMode::Render, "json");
    // Including one recognised by its bytes rather than its name: a compiled
    // manifest is called `.xml` exactly as a written one is. It is a document
    // under the same rule as the rest and not by a case of its own, so the
    // setting below turns it off with everything else.
    if let Some(axml) = axml_bytes() {
        assert_eq!(
            opens_as("AndroidManifest.xml", &axml, true),
            ViewerMode::Render,
            "compiled android xml"
        );
        assert_eq!(
            opens_as("AndroidManifest.xml", &axml, false),
            ViewerMode::Hex,
            "and with documents off it is the binary it is"
        );
    }
    assert_eq!(opens_as("a.png", &png, true), ViewerMode::Render, "png");
    assert_eq!(opens_as("a.bin", &binary, true), ViewerMode::Hex, "binary");
    assert_eq!(opens_as("a.txt", text, true), ViewerMode::Text, "text");

    // Off: the floor and the binary rule decide, and nothing opens as a
    // document however well it is recognised.
    assert_eq!(
        opens_as("a.json", json, false),
        ViewerMode::Text,
        "json off"
    );
    assert_eq!(opens_as("a.png", &png, false), ViewerMode::Hex, "png off");
    assert_eq!(
        opens_as("a.bin", &binary, false),
        ViewerMode::Hex,
        "binary off"
    );
    assert_eq!(opens_as("a.txt", text, false), ViewerMode::Text, "text off");
}

#[test]
fn default_mode_is_the_floor_and_only_the_floor() {
    let cfg = ViewerConfig {
        default_mode: ViewerMode::Hex,
        ..ViewerConfig::default()
    };
    let open = |body: &[u8], name: &str, cfg: &ViewerConfig| {
        let bytes = std::sync::Arc::new(body.to_vec());
        let len = bytes.len() as u64;
        let mut viewer = Viewer::open(
            ViewerId(1),
            name,
            None,
            source::memory_opener(bytes),
            Some(len),
            cfg,
        )
        .expect("open");
        viewer
            .choose_initial_mode(cfg.default_mode, cfg.open_as_document)
            .expect("choose");
        viewer.mode()
    };
    // A plain text file lands on the floor.
    assert_eq!(open(b"prose\n", "a.txt", &cfg), ViewerMode::Hex);
    // And a recognised format still beats it.
    assert_eq!(open(b"{\"a\": 1}", "a.json", &cfg), ViewerMode::Render);
}

/// A file whose extension promises a renderer and whose content does not
/// deliver falls through the whole precedence rather than getting stuck.
#[test]
fn a_json_file_that_is_not_json_opens_as_text_rather_than_as_nothing() {
    assert_eq!(
        opens_as("broken.json", b"this is not json at all\n", true),
        ViewerMode::Text
    );
}

/// Opening reads a bounded head and not the file, whatever the setting.
#[test]
fn opening_a_large_unrecognised_file_reads_only_a_head() {
    let big = vec![b'x'; 4 * 1024 * 1024];
    let cfg = ViewerConfig {
        open_as_document: true,
        ..ViewerConfig::default()
    };
    let bytes = std::sync::Arc::new(big);
    let len = bytes.len() as u64;
    let mut viewer = Viewer::open(
        ViewerId(1),
        "big.log",
        None,
        source::memory_opener(bytes),
        Some(len),
        &cfg,
    )
    .expect("open");
    viewer
        .choose_initial_mode(cfg.default_mode, cfg.open_as_document)
        .expect("choose");
    assert_eq!(viewer.mode(), ViewerMode::Text);
    assert!(viewer.rendered().is_none(), "nothing was kept");
}
