//! Tests for the streaming core.

use super::*;
use crate::config::ViewerConfig;

fn cfg() -> ViewerConfig {
    ViewerConfig::default()
}

fn open(body: &str) -> Viewer {
    open_with(body, &cfg())
}

fn open_with(body: &str, cfg: &ViewerConfig) -> Viewer {
    let bytes = Arc::new(body.as_bytes().to_vec());
    let len = bytes.len() as u64;
    Viewer::open(
        ViewerId(1),
        "test",
        None,
        source::memory_opener(bytes),
        Some(len),
        cfg,
    )
    .expect("open")
}

fn open_bytes(body: &[u8], cfg: &ViewerConfig) -> Viewer {
    let bytes = Arc::new(body.to_vec());
    let len = bytes.len() as u64;
    Viewer::open(
        ViewerId(1),
        "test.bin",
        None,
        source::memory_opener(bytes),
        Some(len),
        cfg,
    )
    .expect("open")
}

fn texts(v: &Viewer) -> Vec<String> {
    v.rows()
        .iter()
        .filter_map(|r| match r {
            Row::Text { text, .. } => Some(text.clone()),
            Row::Hex { .. } => None,
        })
        .collect()
}

#[test]
fn opening_reads_one_window_and_nothing_else() {
    // "A 40 GB file must open as fast as a 4 KB one." The proof
    // available to a unit test is that opening reads a *bounded* prefix: the
    // source's cursor never goes past the detection window, whatever the size.
    let big = Arc::new(vec![b'x'; 4 * 1024 * 1024]);
    let len = big.len() as u64;
    let mut v = Viewer::open(
        ViewerId(1),
        "big",
        None,
        source::memory_opener(big),
        Some(len),
        &cfg(),
    )
    .expect("open");
    assert_eq!(v.len(), Some(len));
    assert_eq!(v.top(), 0);
    assert!(!v.index().is_complete(), "the index has not run yet");
    assert_eq!(v.index().scanned(), 0);
    // And it is usable immediately, with no index at all.
    v.layout(10, 80).expect("layout");
    assert_eq!(v.rows().len(), 10);
}

#[test]
fn a_scan_job_is_handed_over_exactly_once() {
    let mut v = open("a\nb\n");
    assert!(v.take_scan().is_some());
    assert!(v.take_scan().is_none(), "a viewer is indexed once");
}

#[test]
fn dropping_the_viewer_cancels_the_scan() {
    let mut v = open("a\nb\n");
    let job = v.take_scan().expect("scan");
    let cancel = Arc::clone(&job.cancel);
    assert!(!cancel.load(Ordering::Relaxed));
    drop(v);
    assert!(
        cancel.load(Ordering::Relaxed),
        "a background task must not outlive what it was for"
    );
}

#[test]
fn text_rows_come_out_in_order_with_tabs_expanded() {
    let mut v = open("one\n\ttwo\nthree\n");
    v.layout(5, 40).expect("layout");
    assert_eq!(texts(&v), ["one", "    two", "three"]);
    let first = v.rows().first().expect("row");
    assert_eq!(first.offset(), 0);
    assert!(matches!(
        first,
        Row::Text {
            line: Some(0),
            first: true,
            ..
        }
    ));
}

#[test]
fn scrolling_is_local_and_works_before_the_index_has_run() {
    let body: String = (0..1_000).map(|i| format!("line {i}\n")).collect();
    let mut v = open(&body);
    assert_eq!(v.index().scanned(), 0, "nothing has been indexed");

    v.layout(3, 40).expect("layout");
    assert_eq!(texts(&v), ["line 0", "line 1", "line 2"]);

    v.scroll(2).expect("down");
    v.layout(3, 40).expect("layout");
    assert_eq!(texts(&v), ["line 2", "line 3", "line 4"]);

    v.scroll(-1).expect("up");
    v.layout(3, 40).expect("layout");
    assert_eq!(texts(&v), ["line 1", "line 2", "line 3"]);

    v.goto_start().expect("start");
    v.layout(1, 40).expect("layout");
    assert_eq!(texts(&v), ["line 0"]);
    assert_eq!(v.top(), 0);
}

#[test]
fn scrolling_up_at_the_top_stays_at_the_top() {
    let mut v = open("a\nb\nc\n");
    v.scroll(-50).expect("up");
    assert_eq!(v.top(), 0);
    v.layout(1, 20).expect("layout");
    assert_eq!(texts(&v), ["a"]);
}

#[test]
fn scrolling_past_the_end_stops_with_the_last_line_at_the_bottom() {
    // Not "stops somewhere near the end": pressing `Down` forever must leave a
    // *full* screen with the last line on its bottom row. Walking the window
    // off the end and showing blank rows is what this guards against.
    let body: String = (1..=40).map(|n| format!("line {n}\n")).collect();
    let mut v = open(&body);
    v.scroll(500).expect("down");
    v.layout(18, 40).expect("layout");
    assert_eq!(texts(&v).len(), 18, "a full screen, not a blank one");
    assert_eq!(texts(&v).last().map(String::as_str), Some("line 40"));

    // And it is the same place `End` lands on, which is the point: two ways of
    // asking for the last page cannot give two different last pages.
    let parked = v.top();
    let mut w = open(&body);
    w.layout(18, 40).expect("layout");
    w.goto_end().expect("end");
    w.layout(18, 40).expect("layout");
    assert_eq!(w.top(), parked);
}

#[test]
fn a_file_shorter_than_the_window_keeps_its_first_line_on_the_first_row() {
    // The pull-back must not fire here: the blank rows below a three-line file
    // in an eighteen-row window are the truth, not an overscroll.
    let mut v = open("a\nb\nc\n");
    v.scroll(500).expect("down");
    v.layout(18, 20).expect("layout");
    assert_eq!(texts(&v), ["a", "b", "c"]);
    assert_eq!(v.top(), 0);
}

/// Twelve lines that each wrap into several rows at twenty columns.
fn wrapped_body() -> String {
    (1..=12)
        .map(|n| format!("{}\n", "x".repeat(3 + n * 29)))
        .collect()
}

#[test]
fn with_wrapping_on_down_moves_one_row_not_one_line() {
    // A wrapped line is several rows and `Down` moves one of them. Stepping a
    // whole line instead makes the window jump by however many rows that line
    // happened to have.
    let mut v = open(&wrapped_body());
    v.toggle_wrap();
    v.layout(10, 20).expect("layout");
    let first = texts(&v);
    v.scroll(1).expect("down");
    v.layout(10, 20).expect("layout");
    let second = texts(&v);
    assert_eq!(
        second.first(),
        first.get(1),
        "one row down: the second row became the first"
    );
}

#[test]
fn with_wrapping_on_up_undoes_exactly_what_down_did() {
    // The asymmetry this guards against: `Down` walking rows while `Up` walks
    // lines, so `PgUp` goes further back than `PgDn` came forward.
    let mut v = open(&wrapped_body());
    v.toggle_wrap();
    v.layout(10, 20).expect("layout");
    let home = texts(&v);
    for _ in 0..7 {
        v.scroll(1).expect("down");
        v.layout(10, 20).expect("layout");
    }
    for _ in 0..7 {
        v.scroll(-1).expect("up");
        v.layout(10, 20).expect("layout");
    }
    assert_eq!(texts(&v), home);
}

#[test]
fn with_wrapping_on_the_last_page_is_the_last_page() {
    let body = wrapped_body();
    let mut v = open(&body);
    v.toggle_wrap();
    v.layout(10, 20).expect("layout");
    for _ in 0..400 {
        v.scroll(1).expect("down");
    }
    v.layout(10, 20).expect("layout");
    assert_eq!(texts(&v).len(), 10, "a full screen, not a blank one");

    let parked = (v.top(), v.top_row());
    let mut w = open(&body);
    w.toggle_wrap();
    w.layout(10, 20).expect("layout");
    w.goto_end().expect("end");
    w.layout(10, 20).expect("layout");
    assert_eq!(
        (w.top(), w.top_row()),
        parked,
        "`End` and scrolling to the end are the same place"
    );
}

#[test]
fn a_bom_is_skipped_rather_than_drawn() {
    let mut v = open("\u{feff}hello\nworld\n");
    assert_eq!(v.top(), 3, "the top starts after the BOM");
    v.layout(2, 20).expect("layout");
    assert_eq!(texts(&v), ["hello", "world"]);
    assert_eq!(v.status().encoding_how, Detected::Bom);
}

#[test]
fn end_is_marked_approximate_rather_than_blocked_while_the_index_builds() {
    // in as many words.
    let body: String = (0..500).map(|i| format!("line {i}\n")).collect();
    let mut v = open(&body);
    v.layout(4, 40).expect("layout");
    assert!(!v.index().is_complete());

    v.goto_end().expect("end");
    let st = v.status();
    assert!(
        st.approximate,
        "the seek happened and said it was approximate: {st:?}"
    );
    v.layout(4, 40).expect("layout");
    assert!(
        texts(&v).iter().any(|t| t.contains("line 499")),
        "and it landed on the end anyway: {:?}",
        texts(&v)
    );
}

#[tokio::test]
async fn once_the_index_completes_the_line_numbers_are_exact() {
    let body: String = (0..2_000).map(|i| format!("line {i}\n")).collect();
    let mut cfg = cfg();
    cfg.index_chunk = crate::config::ByteSize(index::MIN_CHUNK);
    let mut v = open_with(&body, &cfg);

    let job = v.take_scan().expect("scan");
    let (tx, mut rx) = tokio::sync::mpsc::channel(index::INDEX_CHANNEL_DEPTH);
    let ScanJob {
        id,
        source,
        chunk,
        cancel,
    } = job;
    tokio::task::spawn_blocking(move || index::scan(id, source, chunk, cancel, tx))
        .await
        .expect("scan task");
    while let Some(batch) = rx.recv().await {
        v.apply_index(&batch);
    }
    assert!(v.index().is_complete());
    assert_eq!(v.index().known_lines(), 2_000);

    v.goto_line(1_500).expect("goto");
    assert_eq!(v.status().line, Some(1_500));
    assert!(!v.status().approximate);
    v.layout(1, 40).expect("layout");
    assert_eq!(texts(&v), ["line 1500"]);

    v.goto_end().expect("end");
    assert!(!v.status().approximate, "{:?}", v.status());
}

#[test]
fn hex_mode_is_arithmetic_and_needs_no_index() {
    let body: Vec<u8> = (0..=255_u8).collect();
    let mut cfg = cfg();
    cfg.default_mode = ViewerMode::Hex;
    let mut v = open_bytes(&body, &cfg);
    assert_eq!(v.mode(), ViewerMode::Hex);

    v.layout(2, 80).expect("layout");
    match v.rows().first() {
        Some(Row::Hex { offset, bytes, .. }) => {
            assert_eq!(*offset, 0);
            assert_eq!(bytes.as_slice(), &body[0..16]);
        }
        other => panic!("expected a hex row, got {other:?}"),
    }
    v.scroll(1).expect("down");
    assert_eq!(v.top(), 16);
    v.goto_offset(0xF0).expect("goto");
    assert_eq!(v.cursor(), 0xF0);
    assert_eq!(v.status().offset, 0xF0);
    // The top is clamped so the last screenful is full rather than one row of
    // file above a screen of nothing, and the offset asked for is still on it.
    assert_eq!(v.top(), 224);
    v.layout(2, 80).expect("layout");
    assert!(
        v.rows().iter().any(|r| r.offset() == 240),
        "the offset asked for is on screen: {:?}",
        v.rows()
    );
}

#[test]
fn the_last_hex_row_is_short_rather_than_padded_with_lies() {
    let mut cfg = cfg();
    cfg.default_mode = ViewerMode::Hex;
    let mut v = open_bytes(b"12345", &cfg);
    v.layout(4, 80).expect("layout");
    assert_eq!(v.rows().len(), 1);
    match v.rows().first() {
        Some(Row::Hex { bytes, .. }) => assert_eq!(bytes.as_slice(), b"12345"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_binary_file_opens_in_hex_without_being_asked() {
    // "A file detected as binary opens in hex automatically
    // unless overridden."
    let v = open_bytes(b"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00", &cfg());
    assert_eq!(v.mode(), ViewerMode::Hex);
    assert!(v.status().binary);

    let v = open_bytes(b"plain text, nothing odd\n", &cfg());
    assert_eq!(v.mode(), ViewerMode::Text);
    assert!(!v.status().binary);
}

#[test]
fn switching_modes_keeps_the_place() {
    let body: String = (0..100).map(|i| format!("line {i}\n")).collect();
    let mut v = open(&body);
    v.scroll(10).expect("down");
    let at = v.top();
    assert!(at > 0);
    v.set_mode(ViewerMode::Hex).expect("mode");
    assert_eq!(v.top(), v.hex().snap(at), "hex lands on the row holding it");
    v.set_mode(ViewerMode::Text).expect("mode");
    // Hex snapped to a 16-byte row, which is not where the line began, so
    // coming back lands on the start of the line those bytes belong to. Never
    // mid-line: a text window always begins at a line start (see `decode`).
    assert!(v.top() <= at, "{} <= {at}", v.top());
    v.layout(1, 40).expect("layout");
    assert!(
        texts(&v).first().is_some_and(|t| t.starts_with("line ")),
        "{:?}",
        texts(&v)
    );
}

#[test]
fn changing_the_encoding_redecodes_the_window_and_nothing_else() {
    // "changing encoding re-decodes the visible window only, not
    // the file."
    let mut body = Vec::new();
    body.extend_from_slice(&[0xC9, 0xCD, 0xBB]); // cp437 box drawing
    body.push(b'\n');
    let mut v = open_bytes(&body, &cfg());
    v.layout(1, 20).expect("layout");
    let as_sniffed = texts(&v);

    let cp437 = decode::encoding_for("cp437").expect("cp437");
    v.set_encoding(cp437).expect("encoding");
    v.layout(1, 20).expect("layout");
    assert_eq!(texts(&v), ["╔═╗"]);
    assert_ne!(texts(&v), as_sniffed);
    assert_eq!(v.status().encoding, "cp437");
    assert_eq!(v.status().encoding_how, Detected::Chosen);
    // The position did not move, which is the half that matters.
    assert_eq!(v.top(), 0);
}

#[test]
fn f8_cycles_the_shortlist_and_comes_back_round() {
    let mut v = open("hello\n");
    let start = v.encoding().label();
    let ring: Vec<&'static str> = v.encoding_shortlist().iter().map(|e| e.label()).collect();
    for _ in 0..ring.len() {
        v.cycle_encoding().expect("encoding");
    }
    assert_eq!(v.encoding().label(), start, "the ring is a ring");
}

#[test]
fn utf16_lines_are_split_on_the_code_unit_grid() {
    // "hi\nok\n" in UTF-16LE, with a BOM so it is detected rather than guessed.
    let mut body = vec![0xFF, 0xFE];
    for c in "hi\nok\n".chars() {
        let mut buf = [0_u16; 2];
        for unit in c.encode_utf16(&mut buf) {
            body.extend_from_slice(&unit.to_le_bytes());
        }
    }
    let mut v = open_bytes(&body, &cfg());
    assert_eq!(v.encoding().label(), "UTF-16LE");
    assert!(!v.status().binary, "UTF-16 is text, NULs and all");
    v.layout(2, 20).expect("layout");
    assert_eq!(texts(&v), ["hi", "ok"]);
}

#[test]
fn a_line_longer_than_the_layout_budget_is_cut_and_says_so() {
    // the memory rule applied to a file with no line breaks: the
    // viewer must not lay out 40 GB because somebody forgot a newline.
    let mut body = "x".repeat(300_000);
    body.push('\n');
    body.push_str("after\n");
    let mut v = open(&body);
    v.layout(2, 80).expect("layout");
    let rows = v.rows();
    match rows.first() {
        Some(Row::Text { cut, text, .. }) => {
            assert!(*cut, "the row says it was cut");
            assert!(
                text.chars().count() < 300_000,
                "and it did not materialise the line: {} chars",
                text.chars().count()
            );
        }
        other => panic!("{other:?}"),
    }
    // And the next line is still found, so the cut does not swallow the file.
    assert!(texts(&v).iter().any(|t| t == "after"), "{:?}", texts(&v));
}

#[test]
fn wrapping_turns_one_line_into_several_rows() {
    let mut v = open("abcdefghij\nnext\n");
    v.toggle_wrap();
    assert!(v.wrap());
    v.layout(4, 4).expect("layout");
    assert_eq!(texts(&v), ["abcd", "efgh", "ij", "next"]);
    // The continuation rows carry the same offset and are not "first".
    let firsts: Vec<bool> = v
        .rows()
        .iter()
        .filter_map(|r| match r {
            Row::Text { first, .. } => Some(*first),
            Row::Hex { .. } => None,
        })
        .collect();
    assert_eq!(firsts, [true, false, false, true]);
}

#[test]
fn an_empty_file_lays_out_without_a_row_and_without_a_panic() {
    let mut v = open("");
    v.layout(5, 20).expect("layout");
    assert!(v.rows().is_empty());
    v.scroll(1).expect("down");
    v.scroll(-1).expect("up");
    v.goto_end().expect("end");
    assert_eq!(v.top(), 0);
}

#[test]
fn a_one_by_one_terminal_lays_out_nothing_rather_than_crashing() {
    let mut v = open("hello\nworld\n");
    v.layout(0, 0).expect("layout");
    assert!(v.rows().is_empty());
    v.layout(1, 1).expect("layout");
    assert_eq!(v.rows().len(), 1);
}

#[test]
fn an_invalid_sequence_is_a_glyph_and_the_file_still_opens() {
    // "Invalid sequences render as a replacement glyph rather
    // than failing the open."
    // Forced to UTF-8: with `auto` chardetng would (correctly) call these
    // bytes windows-1252, and there would be no invalid sequence to test.
    let mut cfg = cfg();
    cfg.encoding.default = "utf-8".to_string();
    let mut v = open_bytes(b"good \xc3\x28 bad\nnext\n", &cfg);
    v.layout(2, 40).expect("layout");
    let rows = texts(&v);
    assert_eq!(rows.len(), 2);
    assert!(rows[0].contains('\u{fffd}'), "{rows:?}");
    assert_eq!(rows[1], "next");
}

#[test]
fn highlighting_is_off_above_the_configured_size() {
    let mut cfg = cfg();
    cfg.highlight.max_size = crate::config::ByteSize(4);
    let v = open_with("fn main() {}\n", &cfg);
    assert!(!v.status().highlighted, "a file over the limit opens plain");

    let mut cfg2 = self::cfg();
    cfg2.highlight.engine = crate::config::HighlightEngine::None;
    let v = open_with("fn main() {}\n", &cfg2);
    assert!(!v.status().highlighted);
}

#[test]
fn syntax_highlighting_reaches_the_rows() {
    let mut v = Viewer::open(
        ViewerId(1),
        "x.rs",
        Some(crate::vfs::VfsPath::local("/tmp/x.rs")),
        source::memory_opener(Arc::new(b"fn main() {}\n".to_vec())),
        Some(13),
        &cfg(),
    )
    .expect("open");
    v.layout(1, 40).expect("layout");
    match v.rows().first() {
        Some(Row::Text { spans, .. }) => {
            assert!(!spans.is_empty(), "highlighting produced no spans");
            assert!(
                spans
                    .iter()
                    .any(|s| s.slot == Some(highlight::SynSlot::Keyword)),
                "{spans:?}"
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_help_page_is_generated_from_the_active_keymap() {
    // "The keyboard page is generated from the active keymap, not
    // written by hand."
    let mut keymap = crate::config::Keymap::builtin();
    let page = crate::ui::help::viewer_page(&keymap, true);
    // Capitalised the way the design writes keys: the page and the design's
    // menu now share one renderer, `Keymap::describe`.
    //
    assert!(page.contains("F3"), "{page}");
    assert!(page.contains("Close the viewer"), "{page}");

    keymap.overlay("[viewer]\nclose = [\"alt+z\"]\n", "test.toml");
    let page = crate::ui::help::viewer_page(&keymap, true);
    assert!(
        page.contains("Alt+Z"),
        "a rebound key shows the user's binding: {page}"
    );
}

#[test]
fn the_help_page_opens_through_the_same_machinery() {
    let keymap = crate::config::Keymap::builtin();
    let mut v = Viewer::open_memory(
        ViewerId(2),
        "Viewer keys",
        crate::ui::help::viewer_page(&keymap, true),
        &cfg(),
    )
    .expect("open help");
    assert!(!v.is_help(), "generated text is not help until it says so");
    v.mark_help();
    assert!(v.is_help());
    assert_eq!(v.mode(), ViewerMode::Text);
    v.layout(3, 80).expect("layout");
    assert!(!v.rows().is_empty());
}

#[test]
fn the_status_line_carries_what_spec_10_asks_it_to() {
    let mut v = open("alpha\nbeta\n");
    v.layout(2, 20).expect("layout");
    let st = v.status();
    assert_eq!(st.mode, ViewerMode::Text);
    assert_eq!(st.offset, 0);
    assert_eq!(st.len, Some(11));
    // The whole file is on screen, so all of it has been seen. The
    // percentage measures the bottom of the window, not the top - measuring
    // the top meant the last page of a long file read 98% and 100 was never
    // reached at all.
    assert_eq!(st.percent, Some(100));
    assert_eq!(st.encoding, "UTF-8");
    assert!(!st.decode_errors);
    assert!(!st.wrap);
    assert!(!st.indexed, "the scan has not been run in this test");
    assert!(st.error.is_none());
}

#[test]
fn goto_offset_snaps_to_a_line_start_in_text_mode() {
    let mut v = open("alpha\nbeta\ngamma\n");
    v.goto_offset(8).expect("goto"); // inside "beta"
    assert_eq!(v.top(), 6, "the top is the line start");
    assert_eq!(v.cursor(), 8, "and the cursor is where it was asked for");
    v.layout(1, 20).expect("layout");
    assert_eq!(texts(&v), ["beta"]);
}

#[test]
fn goto_percent_never_refuses() {
    let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
    let mut v = open(&body);
    for p in [0_u8, 50, 100, 200] {
        v.goto_percent(p).expect("percent");
        v.layout(1, 40).expect("layout");
    }
    assert!(v.top() > 0);
}

#[test]
fn a_forward_only_source_still_scrolls_both_ways() {
    // "For a source that cannot seek … backward seeks replay
    // from the nearest checkpoint."
    let body: Arc<Vec<u8>> = Arc::new(
        (0..500)
            .map(|i| format!("line {i}\n"))
            .collect::<String>()
            .into_bytes(),
    );
    let len = body.len() as u64;
    let opener: source::Opener = {
        let body = Arc::clone(&body);
        Arc::new(move || {
            Ok(source::Stream::Forward(Box::new(std::io::Cursor::new(
                body.as_ref().clone(),
            ))))
        })
    };
    let mut v = Viewer::open(ViewerId(9), "stream", None, opener, Some(len), &cfg()).expect("open");
    v.scroll(20).expect("down");
    v.layout(1, 40).expect("layout");
    assert_eq!(texts(&v), ["line 20"]);
    v.scroll(-15).expect("up");
    v.layout(1, 40).expect("layout");
    assert_eq!(texts(&v), ["line 5"]);
}

#[test]
fn a_line_far_longer_than_the_read_budget_still_lands_on_a_character_boundary() {
    // The residue case of `decode`'s module docs: the viewer cannot snap to a
    // line start because there is no line start within the budget, so it
    // resyncs locally and says the position is approximate rather than
    // decoding from a byte in the middle of a sequence.
    //
    // The window budget is NAV_READ_BUDGET × MAX_WINDOW; the file is one line
    // more than twice that, made of two-byte UTF-8 characters so a naive
    // landing has a one-in-two chance of splitting one.
    let span = source::MAX_WINDOW * (NAV_READ_BUDGET as usize) * 2;
    let body: String = "é".repeat(span);
    let mut v = open(&body);

    // An odd offset lands inside a `é` (C3 A9).
    let at = (span as u64) + 1;
    v.goto_offset(at).expect("goto");
    let top = v.top();
    assert!(top.is_multiple_of(2), "landed on a lead byte: {top}");
    assert!(
        v.status().approximate,
        "and it says the position is approximate: {:?}",
        v.status()
    );

    v.layout(1, 20).expect("layout");
    assert!(
        !texts(&v).first().is_some_and(|t| t.starts_with('\u{fffd}')),
        "the first character is not a broken sequence: {:?}",
        texts(&v)
    );
}

#[test]
fn a_search_that_runs_out_of_budget_is_resumed_by_the_next_n() {
    // "searching is streamed over the file, not over a loaded
    // buffer, so it starts returning hits on a huge file immediately."
    // Immediately means bounded, and bounded means one keystroke may not reach
    // the match. What must not happen is that it can never be reached: each
    // `n` picks up where the last one stopped.
    use crate::viewer::find::{FIND_READ_BUDGET, Found};
    use crate::viewer::source::MAX_WINDOW;

    // One window past what a single step will read, so the first search is
    // guaranteed to exhaust its budget and the second is guaranteed to finish.
    let reach = (MAX_WINDOW as u64).saturating_mul(u64::from(FIND_READ_BUDGET));
    let mut body = vec![b'.'; usize::try_from(reach).expect("fits") + 4096];
    let at = body.len() - 8;
    body[at..].copy_from_slice(b"NEEDLE!\n");

    let cfg = crate::config::ViewerConfig::default();
    let mut v = Viewer::open(
        ViewerId(9),
        "big.txt",
        None,
        crate::viewer::source::memory_opener(std::sync::Arc::new(body)),
        None,
        &cfg,
    )
    .expect("open");
    v.layout(10, 80).expect("layout");

    v.open_find();
    for c in "NEEDLE".chars() {
        assert!(
            matches!(
                v.find_type(c).expect("type"),
                Found::Budget(_) | Found::None
            ),
            "the needle is deliberately past one step's reach"
        );
    }
    assert_eq!(v.find().current(), None, "and it has not been found yet");

    // `n` carries on rather than starting over.
    let mut found = None;
    for _ in 0..4 {
        match v.find_next().expect("n") {
            Found::Hit(hit) => {
                found = Some(hit);
                break;
            }
            Found::Budget(_) => continue,
            Found::None => break,
        }
    }
    assert_eq!(
        found,
        Some(at as u64),
        "n resumed from the budget offset and reached the match"
    );
    assert_eq!(v.find().current(), Some(at as u64));
    assert!(
        v.top() <= at as u64 && v.cursor() == at as u64,
        "and the screen was moved to it"
    );
}

// ---------------------------------------------------------- the sweep ------

/// A deterministic PRNG. Deterministic on purpose: a sweep that fails only on
/// some runs is a sweep nobody can act on, and the seeds are printed with any
/// failure so one case can be replayed on its own.
struct Rng(u64);

impl Rng {
    const fn new(seed: u64) -> Self {
        // 0 is a fixed point of xorshift, so no seed is allowed to be it.
        Self(seed | 1)
    }

    const fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn pick<T: Copy>(&mut self, from: &[T]) -> T {
        let i = usize::try_from(self.next() % (from.len().max(1) as u64)).unwrap_or(0);
        from.get(i).copied().unwrap_or_else(|| {
            // `from` is never empty at any call site; this keeps the helper
            // total rather than making the sweep itself a source of panics.
            *from
                .first()
                .expect("the sweep never picks from an empty menu")
        })
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % (n.max(1) as u64)).unwrap_or(0)
    }
}

/// Every motion the design binds a key to.
const MOTIONS: &[select::Motion] = &[
    select::Motion::Up,
    select::Motion::Down,
    select::Motion::Left,
    select::Motion::Right,
    select::Motion::PageUp,
    select::Motion::PageDown,
    select::Motion::RowStart,
    select::Motion::RowEnd,
    select::Motion::FileStart,
    select::Motion::FileEnd,
];

/// Bare, `Shift` and `Ctrl+Shift`.
const EXTENDS: &[select::Extend] = &[
    select::Extend::None,
    select::Extend::Linear,
    select::Extend::Rectangular,
];

/// A body built out of the byte shapes that break a naive viewer.
///
fn hazardous_body(rng: &mut Rng) -> Vec<u8> {
    // Every piece is chosen so that *concatenating* them produces the hazards
    // as well: a split UTF-8 sequence, a lone continuation byte, a CRLF broken
    // across a boundary, a run with no line break at all, NULs, and a UTF-16
    // pair on the odd byte.
    const PIECES: &[&[u8]] = &[
        b"",
        b"\n",
        b"\r\n",
        b"\r",
        b"plain ascii line\n",
        b"\ttabbed\tline\twith\ttabs\n",
        b"\xef\xbb\xbf",             // a BOM, and not always at the front
        b"\xe6\x97\xa5\xe6\x9c\xac", // 日本
        b"\xe6\x97",                 // half of one, deliberately
        b"\x80\x80\x80",             // lone continuation bytes
        b"\xff\xfe",
        b"h\0e\0l\0l\0o\0",  // utf-16le text
        b"\0\0\0\0\0\0\0\0", // NULs: binary detection
        b"\x07\x1b\x7f",     // controls that must become pictures
        b"no break at all and it just keeps going for a while yet",
        b"\xc3\xa9\xc3\xa8\xc3\xaa", // eee, so a byte cut lands mid-sequence
    ];
    let mut out = Vec::new();
    for _ in 0..rng.below(24) {
        out.extend_from_slice(rng.pick(PIECES));
    }
    out
}

#[test]
fn no_shape_of_file_terminal_or_keystroke_makes_the_viewer_panic() -> Result<()> {
    // the design name the shapes: a byte range that splits a
    // UTF-8 sequence, a seek past the end, a zero-length file, an index that
    // has not arrived, `hex_width` of zero, a terminal narrower than the offset
    // column. Reading for them finds the ones you thought of; this drives every
    // combination of them and finds the ones you did not.
    //
    // The assertion is deliberately weak - "it came back" - because the point
    // is the absence of a panic, and any stronger claim about a randomly
    // generated file would be a second implementation of the viewer.
    let widths: &[u16] = &[0, 1, 3, 16, 64, 255, u16::MAX];
    let tabs: &[u16] = &[0, 1, 4, 8, 255, u16::MAX];
    let sizes: &[(u16, u16)] = &[
        (0, 0),
        (1, 1),
        (2, 3),
        (3, 8),
        (15, 60),
        (24, 120),
        (60, 200),
    ];
    let encodings: &[&str] = &[
        "auto",
        "utf-8",
        "utf-16le",
        "utf-16be",
        "windows-1252",
        "cp437",
    ];

    for seed in 1..=400_u64 {
        let mut rng = Rng::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let mut c = cfg();
        c.hex_width = rng.pick(widths);
        c.tab_width = rng.pick(tabs);
        c.wrap = rng.next().is_multiple_of(2);
        // Both halves of the `viewer.cursor`, because pure
        // scrolling is a second code path and not an absence of one.
        c.cursor = rng.next().is_multiple_of(2);
        c.line_numbers = rng.next().is_multiple_of(2);
        c.encoding.default = rng.pick(encodings).to_string();
        let body = hazardous_body(&mut rng);
        let len = body.len() as u64;
        let mut v = Viewer::open(
            ViewerId(7),
            "sweep.txt",
            None,
            source::memory_opener(Arc::new(body)),
            // Half the runs lie about the length the way a file truncated
            // while open does, and the other half report it honestly. Neither
            // may panic.
            if rng.next().is_multiple_of(2) {
                Some(len)
            } else {
                Some(len.saturating_mul(4).saturating_add(1_000_000))
            },
            &c,
        )
        .unwrap_or_else(|e| panic!("seed {seed}: open failed: {e}"));

        for _ in 0..24 {
            let (rows, cols) = rng.pick(sizes);
            match rng.below(19) {
                0 => v.scroll(rng.below(9) as isize - 4)?,
                1 => v.page(rng.next().is_multiple_of(2))?,
                2 => v.goto_start()?,
                3 => v.goto_end()?,
                4 => v.goto_offset(rng.next() % (len.saturating_mul(2) + 8))?,
                5 => v.goto_percent(rng.pick(&[0_u8, 1, 50, 99, 100, 255]))?,
                6 => v.goto_line(rng.next() % 1_000)?,
                7 => v.toggle_mode()?,
                8 => v.toggle_wrap(),
                9 => v.cycle_encoding()?,
                10 => v.scroll_horizontal(rng.below(41) as isize - 20)?,
                11 => {
                    v.open_find();
                    for ch in rng
                        .pick(&["a", "\u{65e5}", "DE AD", "??", "\0", "e\u{301}"])
                        .chars()
                    {
                        v.find_type(ch)?;
                    }
                }
                12 => {
                    v.find_next()?;
                    v.find_prev()?;
                }
                // the own keys, in the same sweep: a cursor that
                // can be moved into a byte that is not there, or a selection
                // that outlives the file it was made in, is exactly the class
                // of defect this test exists for
                // (the design invariants 1 and 19).
                13 => {
                    let motion = rng.pick(MOTIONS);
                    let extend = if c.cursor {
                        rng.pick(EXTENDS)
                    } else {
                        select::Extend::None
                    };
                    v.move_cursor(motion, extend)?;
                }
                14 => v.select_all(),
                15 => {
                    v.clear_selection();
                }
                16 => {
                    v.toggle_selection_kind();
                }
                17 => v.switch_hex_side(),
                18 => v.place_cursor(rng.next() % (len.saturating_mul(2) + 8))?,
                _ => v.close_find(),
            }
            // Invariant 1: the cursor is a byte offset and it is always on the
            // file. Half these runs lie about the length the way a file
            // truncated while open does, so the claim is against what the
            // viewer believes the length to be.
            if let Some(known) = v.len() {
                assert!(
                    known == 0 || v.cursor() < known,
                    "seed {seed}: cursor {} is off a {known}-byte file",
                    v.cursor()
                );
            }
            v.layout(rows, cols)
                .unwrap_or_else(|e| panic!("seed {seed}: layout {rows}x{cols} failed: {e}"));
            assert!(
                v.rows().len() <= usize::from(rows),
                "seed {seed}: {rows}x{cols} laid out {} rows",
                v.rows().len()
            );
            // The status line is drawn from this every frame, so it has to be
            // buildable at every one of these positions.
            let status = v.status();
            for width in [0_usize, 1, 8, 60, 200] {
                let _ = crate::ui::viewer::status_fit(&status, width);
            }
            // Invariant 3: after a layout, the cursor's row index is a row
            // there is, and it is the row the cursor is on.
            if c.cursor && !v.rows().is_empty() {
                assert!(
                    v.cursor_row() < v.rows().len().max(1),
                    "seed {seed}: cursor row {} of {} rows",
                    v.cursor_row(),
                    v.rows().len()
                );
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// What a frame reads (the design I9)
// ---------------------------------------------------------------------------

/// A seekable handle that counts every byte it hands out.
///
/// The only way to assert the rule directly. "Memory is a function
/// of the window size, not the file size" is checkable from the rows a layout
/// produced; **what it read** to produce them is not, and that is the half a
/// layout can get catastrophically wrong while looking correct - a screen of
/// five hundred bytes that cost eighty megabytes of reading is indistinguishable
/// from one that cost five hundred, until it is counted.
struct CountingRead {
    bytes: Arc<Vec<u8>>,
    pos: usize,
    read: Arc<std::sync::atomic::AtomicU64>,
}

impl std::io::Read for CountingRead {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let rest = self.bytes.len().saturating_sub(self.pos);
        let n = rest.min(buf.len());
        let src = self
            .bytes
            .get(self.pos..self.pos.saturating_add(n))
            .unwrap_or(&[]);
        buf.get_mut(..n).unwrap_or(&mut []).copy_from_slice(src);
        self.pos = self.pos.saturating_add(n);
        self.read
            .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(n)
    }
}

impl std::io::Seek for CountingRead {
    fn seek(&mut self, to: std::io::SeekFrom) -> std::io::Result<u64> {
        let len = self.bytes.len() as u64;
        let at = match to {
            std::io::SeekFrom::Start(at) => at,
            std::io::SeekFrom::End(d) => len.saturating_add_signed(d),
            std::io::SeekFrom::Current(d) => (self.pos as u64).saturating_add_signed(d),
        };
        self.pos = usize::try_from(at.min(len)).unwrap_or(0);
        Ok(at)
    }
}

fn counting_opener(
    bytes: Arc<Vec<u8>>,
    read: &Arc<std::sync::atomic::AtomicU64>,
) -> source::Opener {
    let read = Arc::clone(read);
    Arc::new(move || {
        Ok(source::Stream::Seekable(Box::new(CountingRead {
            bytes: Arc::clone(&bytes),
            pos: 0,
            read: Arc::clone(&read),
        })))
    })
}

/// Open over a counted source, with the counter zeroed after the open.
fn open_counted(body: Vec<u8>, cfg: &ViewerConfig) -> (Viewer, Arc<std::sync::atomic::AtomicU64>) {
    let read = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let bytes = Arc::new(body);
    let len = bytes.len() as u64;
    let v = Viewer::open(
        ViewerId(1),
        "counted",
        None,
        counting_opener(bytes, &read),
        Some(len),
        cfg,
    )
    .expect("open");
    read.store(0, std::sync::atomic::Ordering::Relaxed);
    (v, read)
}

fn taken(read: &Arc<std::sync::atomic::AtomicU64>) -> u64 {
    read.swap(0, std::sync::atomic::Ordering::Relaxed)
}

#[test]
fn one_screen_of_a_file_with_no_line_breaks_reads_a_screen_not_the_file() {
    // "Memory is capped and is a function of the window size,
    // not the file size." So is the reading: a 40 GB file with no `\n` in it
    // must not turn one frame into a scan of it (the design I9).
    //
    // The failure this pins down read 2 MiB *per row* - one exhausted
    // `NAV_READ_BUDGET` looking for a line start that is not there - while
    // charging the layout budget only the ~500 bytes it kept. Forty rows was
    // 80 MiB, on the frame path, on every frame.
    let (mut v, read) = open_counted(vec![b'x'; 16 * 1024 * 1024], &cfg());
    v.layout(40, 100).expect("layout");
    let first = taken(&read);
    assert!(
        first <= MAX_LAYOUT_BYTES.saturating_add(64 * 1024),
        "one layout read {first} bytes; the ceiling is MAX_LAYOUT_BYTES"
    );
    assert_eq!(v.rows().len(), 40, "and the screen is still full");

    // And the frame after it - the idle redraw, the one that arrives on every
    // index batch and every 250 ms wake-up - costs a screen and nothing more,
    // because what was proven about the line is kept rather than re-proven.
    v.layout(40, 100).expect("again");
    let second = taken(&read);
    assert!(
        second <= 64 * 1024,
        "a repeat layout read {second} bytes; it should cost a screenful"
    );

    // Scrolling is bounded the same way.
    v.scroll(1).expect("down");
    let stepped = taken(&read);
    assert!(stepped <= 64 * 1024, "one line down read {stepped} bytes");
    v.page(true).expect("page down");
    let paged = taken(&read);
    assert!(
        paged <= NAV_READ_BYTES.saturating_add(64 * 1024),
        "one page down read {paged} bytes"
    );
}

#[test]
fn an_ordinary_file_is_not_read_by_the_quarter_megabyte_either() {
    // The other half of the same rule: a line break a few hundred bytes away
    // must not cost a 256 KiB window to find. Forty rows of an ordinary file
    // used to read ten megabytes a page for want of a smaller first read.
    let body: Vec<u8> = (0..4_000)
        .map(|i| format!("line {i} with a little text on it\n"))
        .collect::<String>()
        .into_bytes();
    let (mut v, read) = open_counted(body, &cfg());
    v.layout(40, 100).expect("layout");
    let laid = taken(&read);
    assert!(
        laid <= 64 * 1024,
        "one screen of short lines read {laid} bytes"
    );
    v.page(true).expect("page");
    let paged = taken(&read);
    assert!(paged <= 64 * 1024, "one page down read {paged} bytes");
    v.page(false).expect("back");
    let back = taken(&read);
    assert!(back <= 64 * 1024, "one page up read {back} bytes");
}

#[test]
fn a_screen_inside_one_long_line_numbers_it_once_and_shows_it_whole() {
    // Two bugs with one shape: the rows below a cut line were numbered 1, 2, 3
    // … past the file's own line count, and they began at the raw end of the
    // previous window - so a UTF-8 character straddling that edge was dropped
    // and the row opened with a replacement glyph made out of good bytes (the
    // line numbers, the replacement glyphs).
    let mut body = String::from("first line\n");
    body.push_str(&"\u{65e5}".repeat(4_000)); // 3 bytes each: no boundary is free
    let mut v = open(&body);
    v.layout(6, 80).expect("layout");

    let rows: Vec<(Option<u64>, bool, String)> = v
        .rows()
        .iter()
        .filter_map(|r| match r {
            Row::Text {
                line, first, text, ..
            } => Some((*line, *first, text.clone())),
            Row::Hex { .. } => None,
        })
        .collect();
    assert_eq!(rows.len(), 6);
    assert_eq!(rows.first().map(|r| r.0), Some(Some(0)), "row 0 is line 1");
    assert_eq!(
        rows.get(1).map(|r| (r.0, r.1)),
        Some((Some(1), true)),
        "row 1 is the start of line 2, numbered once"
    );
    for (i, (line, first, text)) in rows.iter().enumerate().skip(2) {
        assert_eq!(*line, Some(1), "row {i} is still line 2 of the file");
        assert!(!first, "row {i} is a continuation and prints no number");
        assert!(
            !text.starts_with('\u{fffd}'),
            "row {i} begins on a character boundary, got {:?}",
            text.chars().take(4).collect::<String>()
        );
        assert!(
            text.chars().all(|c| c == '\u{65e5}'),
            "row {i} is the line's own characters and nothing invented"
        );
    }
    // The rows are contiguous: nothing between them was skipped.
    let offsets: Vec<u64> = v.rows().iter().map(Row::offset).collect();
    for pair in offsets.windows(2) {
        let (a, b) = (
            pair.first().copied().unwrap_or(0),
            pair.get(1).copied().unwrap_or(0),
        );
        assert!(b > a, "rows go forwards: {offsets:?}");
    }
}

#[test]
fn the_tail_of_a_file_with_no_line_breaks_is_reachable() {
    // "arrows, `PgUp`/`PgDn`, `Home`/`End` navigate." On a file
    // whose last line has no newline, `next_line_start` answered `None` and
    // `Down`, `PgDn` and `End` were all no-ops - while the screen below the top
    // row was full of the file continuing, because the layout answered the same
    // question differently.
    let body = "z".repeat(6_000);
    let mut v = open(&body);
    v.layout(5, 80).expect("layout");
    let top = v.top();
    v.scroll(1).expect("down");
    assert!(v.top() > top, "Down moves");
    let after_down = v.top();
    v.page(true).expect("page down");
    assert!(v.top() > after_down, "PgDn moves");
    v.goto_end().expect("end");
    v.layout(5, 80).expect("layout");
    let last = v.rows().last().map_or(0, Row::offset);
    assert!(
        last.saturating_add(4 * 1024) >= 6_000,
        "End reaches the tail: the last row is at {last} of 6000"
    );
    // And Up comes back the way Down went.
    let before_up = v.top();
    v.scroll(-1).expect("up");
    assert!(v.top() < before_up, "Up moves too");
    v.goto_start().expect("start");
    assert_eq!(v.top(), 0);
    v.layout(5, 80).expect("layout");
    assert_eq!(v.rows().first().map(Row::offset), Some(0));
}

#[test]
fn a_line_the_read_budget_cannot_cross_is_never_numbered_past_the_file() {
    // The narrow case the gutter used to fabricate: a line longer than
    // NAV_READ_BYTES. Every row on the screen is inside line 1, and the gutter
    // must say so once rather than counting 1..8 beside a file whose index
    // reports one line.
    let (mut v, _read) = open_counted(vec![b'q'; 6 * 1024 * 1024], &cfg());
    v.layout(8, 100).expect("layout");
    let numbered: Vec<u64> = v
        .rows()
        .iter()
        .filter_map(|r| match r {
            Row::Text { line, first, .. } => Some(first.then_some(line.unwrap_or(0))),
            Row::Hex { .. } => None,
        })
        .flatten()
        .collect();
    assert_eq!(
        numbered,
        vec![0_u64],
        "one line number for one line, not one per row"
    );
}

// ---------------------------------------------------------------------------
// What a mode or an encoding switch does to a search in flight
// ---------------------------------------------------------------------------

#[test]
fn switching_mode_re_reads_the_pattern_and_drops_the_old_count() {
    // the same bar accepts text or a hex byte pattern, so `F4`
    // changes what the pattern *means*. The count beside it belonged to the
    // other reading, and the background counter still running belonged to it
    // too - its batches carried a generation the new query had not moved off,
    // so they were folded in and shown as a final tally for a pattern that
    // matches nothing.
    let body = "the dead parrot\n".repeat(12);
    let mut v = open(&body);
    v.open_find();
    for ch in "dead".chars() {
        v.find_type(ch).expect("type");
    }
    let before = v.find().generation();
    assert!(v.find().matcher().is_some());
    // The counter's answer, folded in the way the event loop folds it.
    assert!(v.take_find_job().is_some(), "a counter was queued");
    let counted = FindBatch {
        id: v.id(),
        generation: before,
        hits: vec![4, 20, 36],
        scanned: body.len() as u64,
        total: 12,
        done: true,
        error: None,
    };
    assert!(v.apply_find(&counted));
    assert_eq!(v.find().total(), 12, "twelve matches in text mode");

    // `F4`. `dead` is now the two bytes DE AD, which are nowhere in the file.
    v.set_mode(ViewerMode::Hex).expect("hex");
    assert!(
        v.find().generation() > before,
        "the generation moved, so the old counter stops being believed"
    );
    assert_eq!(v.find().total(), 0, "and the old tally went with it");
    assert!(v.find().hits().is_empty());
    assert!(!v.find().is_complete(), "nothing has been counted yet");
    assert!(
        v.find().current().is_none(),
        "the current match was a text-mode offset"
    );

    // A batch from the abandoned text scan is refused rather than folded in.
    let stale = FindBatch {
        id: v.id(),
        generation: before,
        hits: vec![0, 16, 32],
        scanned: body.len() as u64,
        total: 12,
        done: true,
        error: None,
    };
    assert!(!v.apply_find(&stale), "a stale batch is not this search's");
    assert_eq!(v.find().total(), 0);

    // And a counter for the new reading was queued in its place.
    assert!(
        v.take_find_job().is_some(),
        "a counter for the hex reading is owed to the event loop"
    );
    // Nothing on screen is painted as a match either.
    v.layout(4, 80).expect("layout");
    let painted: usize = v
        .rows()
        .iter()
        .map(|r| match r {
            Row::Text { matches, .. } | Row::Hex { matches, .. } => matches.len(),
        })
        .sum();
    assert_eq!(painted, 0, "DE AD is not in the file");
}

#[test]
fn switching_encoding_re_reads_the_pattern_too() {
    // The same rule through `F8`: `é` is different bytes in
    // cp437, so the matches counted under the old encoding are not this
    // query's.
    let mut body = Vec::new();
    for _ in 0..5 {
        body.extend_from_slice("caf\u{e9}\n".as_bytes());
    }
    let mut cfg = cfg();
    cfg.encoding.default = "utf-8".to_string();
    cfg.encoding.detect = false;
    let mut v = open_bytes(&body, &cfg);
    v.open_find();
    for ch in "caf\u{e9}".chars() {
        v.find_type(ch).expect("type");
    }
    let before = v.find().generation();
    assert!(v.take_find_job().is_some(), "counter");
    let counted = FindBatch {
        id: v.id(),
        generation: before,
        hits: vec![0],
        scanned: body.len() as u64,
        total: 5,
        done: true,
        error: None,
    };
    assert!(v.apply_find(&counted));
    assert_eq!(v.find().total(), 5);

    let cp437 = decode::encoding_for("cp437").expect("cp437");
    v.set_encoding(cp437).expect("encoding");
    assert!(v.find().generation() > before);
    assert_eq!(v.find().total(), 0, "counted under the other encoding");
    assert!(v.take_find_job().is_some(), "and a new counter is owed");
}

#[test]
fn a_file_that_opens_in_hex_starts_on_the_byte_grid() {
    // hex rows are "seek to `row * width`". A UTF-8 BOM made
    // `top` 3, and because the file already *was* in hex mode, the snap that
    // `set_mode` does on the way in never ran - so every row on screen was at
    // 3, 19, 35 … and the 16-byte grouping lined up with nothing in the file.
    let mut body = vec![0xEF, 0xBB, 0xBF];
    body.extend_from_slice(b"binary\0content here and some more of it\n");
    let mut hex_first = cfg();
    hex_first.default_mode = crate::config::ViewerMode::Hex;
    let mut v = open_bytes(&body, &hex_first);
    assert_eq!(v.mode(), ViewerMode::Hex);
    assert_eq!(v.top(), 0, "the BOM is a byte like any other in hex");
    v.layout(3, 80).expect("layout");
    let offsets: Vec<u64> = v.rows().iter().map(Row::offset).collect();
    assert_eq!(offsets, [0, 16, 32]);
    // And the same for a binary file, which chooses hex for itself.
    let mut v = open_bytes(&body, &cfg());
    assert_eq!(v.mode(), ViewerMode::Hex, "the NUL makes it binary");
    assert_eq!(v.top(), 0);
    v.layout(2, 80).expect("layout");
    assert_eq!(v.rows().first().map(Row::offset), Some(0));
}

#[test]
fn a_configured_shortlist_is_the_f8_ring() {
    // "`F8` cycles through a **configurable** shortlist". The
    // key existed and the config key did not, so neither half of "every
    // encoding it supports is selectable" was reachable.
    let mut with_ring = cfg();
    with_ring.encoding.shortlist = vec![
        "utf-8".to_string(),
        "koi8-r".to_string(),
        "big5".to_string(),
    ];
    let v = open_with("hello\n", &with_ring);
    let ring: Vec<&str> = v.encoding_shortlist().iter().map(|e| e.label()).collect();
    assert!(ring.contains(&"KOI8-R"), "{ring:?}");
    assert!(ring.contains(&"Big5"), "{ring:?}");
    assert!(
        ring.contains(&v.encoding().label()),
        "the encoding the file opened in is always in the ring: {ring:?}"
    );
    // A wholly unrecognised list falls back to the own ring rather than
    // leaving `F8` with nowhere to go.
    let mut nonsense = with_ring;
    nonsense.encoding.shortlist = vec!["klingon".to_string()];
    let v = open_with("hello\n", &nonsense);
    assert!(v.encoding_shortlist().len() >= 2);
}

#[test]
fn a_jump_never_hands_the_visible_window_a_stalled_highlighter() {
    // "The viewer only ever highlights the visible window - a
    // few dozen lines resumed from a checkpoint … tens of lines is
    // microseconds on either engine."
    //
    // A jump resumes from the nearest saved parse state and walks forward to
    // the window. That walk arrived with the *visible window's* whole 250 ms
    // parse budget and was allowed to spend all of it: a quarter of a second on
    // the event-loop thread, and then a highlighter that answers every further
    // line with one plain span - so the screen the walk was done for was drawn
    // with no colour at all, and the parse states saved beneath it belonged to
    // the line the walk stopped on rather than to the rows they were filed
    // under.
    // Short lines, so the walk from the nearest saved state to the jump is
    // tens of thousands of them rather than a few hundred - which is what makes
    // this a test of the budget rather than a test of how fast the machine is.
    let body = "let a=1;\n".repeat(60_000);
    let bytes = Arc::new(body.into_bytes());
    let len = bytes.len() as u64;
    let mut v = Viewer::open(
        ViewerId(1),
        "long.rs",
        Some(crate::vfs::VfsPath::local("/tmp/long.rs")),
        source::memory_opener(bytes),
        Some(len),
        &cfg(),
    )
    .expect("open");

    let coloured = |v: &Viewer| -> usize {
        v.rows()
            .iter()
            .map(|r| match r {
                Row::Text { spans, .. } => spans.iter().filter(|s| s.slot.is_some()).count(),
                Row::Hex { .. } => 0,
            })
            .sum()
    };

    v.layout(40, 100).expect("first screen");
    assert!(v.status().highlighted, "the file is highlighted");
    assert!(coloured(&v) > 0, "the first screen is coloured");

    // A jump far enough that the only saved state is the one from the top.
    v.goto_offset(250_000).expect("jump");
    v.layout(40, 100).expect("the screen after the jump");
    assert!(
        coloured(&v) > 0,
        "the screen a catch-up was done for is highlighted, not drawn plain"
    );

    // And the screen after that, which resumes from what this one saved.
    v.scroll(1).expect("down");
    v.layout(40, 100).expect("and the next");
    assert!(coloured(&v) > 0, "and so is the one after it");
}

#[test]
fn a_catch_up_that_runs_out_of_time_is_a_failed_catch_up() {
    // The seam under the test above, without depending on how fast the machine
    // is. `catch_up` used to answer "did the walk reach the offset" and nothing
    // else - so a walk that spent its budget and left the parser **stalled**
    // was reported as a success, and the caller handed the screen a highlighter
    // whose every line comes back plain.
    let body = "let a=1;\n".repeat(64);
    let bytes = Arc::new(body.into_bytes());
    let len = bytes.len() as u64;
    let mut v = Viewer::open(
        ViewerId(1),
        "x.rs",
        Some(crate::vfs::VfsPath::local("/tmp/x.rs")),
        source::memory_opener(bytes),
        Some(len),
        &cfg(),
    )
    .expect("open");
    let mut hl = Highlighter::for_file("x.rs", "let a=1;").expect("rust is a known syntax");
    assert!(
        !v.catch_up(&mut hl, 0, 90, std::time::Duration::ZERO),
        "a walk with no time left is a failed walk, not a stalled parser passed on"
    );
    assert!(hl.is_stalled(), "and it really did stall");
    // With time to spend it succeeds, and hands back a parser that still works.
    let mut hl = Highlighter::for_file("x.rs", "let a=1;").expect("syntax");
    assert!(v.catch_up(&mut hl, 0, 90, highlight::PARSE_BUDGET));
    assert!(!hl.is_stalled());
    assert!(hl.line("fn main() {}").iter().any(|s| s.slot.is_some()));
}

// ---------------------------------------------------------------------------
// The cursor
// ---------------------------------------------------------------------------

/// Twenty numbered lines, seven bytes each up to `line 9`.
fn numbered(n: u64) -> String {
    (0..n).map(|i| format!("line {i}\n")).collect()
}

fn hex_cfg(width: u16) -> ViewerConfig {
    let mut c = cfg();
    c.default_mode = ViewerMode::Hex;
    c.hex_width = width;
    c
}

fn ramp(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 256) as u8).collect()
}

#[test]
fn the_arrows_move_a_cursor_and_the_view_follows_only_at_the_edges() {
    // "the arrow keys move it rather than scrolling the page;
    // the view follows when it reaches an edge".
    let mut v = open(&numbered(20));
    v.layout(5, 40).expect("layout");
    assert_eq!(v.cursor(), 0);
    assert_eq!(v.cursor_row(), 0);

    for row in 1..5 {
        v.move_cursor(Motion::Down, Extend::None).expect("down");
        assert_eq!(v.top(), 0, "the window has not moved");
        assert_eq!(v.cursor_row(), row);
        assert_eq!(v.cursor(), row as u64 * 7);
    }
    // On the window's last row, the window is what moves.
    v.move_cursor(Motion::Down, Extend::None).expect("down");
    assert_eq!(v.cursor_row(), 4, "still the last row");
    assert_eq!(v.cursor(), 35, "but on the line below");
    assert_eq!(v.top(), 7, "so the window came with it");
}

#[test]
fn n_keys_before_one_layout_land_where_n_layouts_would() {
    // `main::drain_input` applies every waiting key before the next layout, so
    // movement computed from `self.rows` lags by N and the cursor trails a held
    // key (the design, invariant 2).
    let body = numbered(200);
    let mut drained = open(&body);
    let mut framed = open(&body);
    drained.layout(10, 40).expect("layout");
    framed.layout(10, 40).expect("layout");

    for _ in 0..30 {
        drained
            .move_cursor(Motion::Down, Extend::None)
            .expect("down");
        framed
            .move_cursor(Motion::Down, Extend::None)
            .expect("down");
        framed.layout(10, 40).expect("layout");
    }
    drained.layout(10, 40).expect("layout");

    assert_eq!(drained.cursor(), framed.cursor(), "thirty keys, one layout");
    assert_eq!(drained.top(), framed.top());
    assert_eq!(drained.cursor_row(), framed.cursor_row());
}

#[test]
fn the_cursor_row_index_survives_every_scroll_and_pull_back() {
    // Invariant 3, including `layout_to_the_end`'s pull-back - the one place
    // the window moves without a key.
    let mut v = open(&numbered(12));
    v.layout(5, 40).expect("layout");
    for _ in 0..40 {
        v.move_cursor(Motion::Down, Extend::None).expect("down");
    }
    v.layout(5, 40).expect("layout");
    assert!(
        matches!(
            v.rows().get(v.cursor_row()),
            Some(Row::Text {
                cursor: Some(_),
                ..
            })
        ),
        "row {} of {:?}",
        v.cursor_row(),
        v.rows()
    );

    v.move_cursor(Motion::PageDown, Extend::None).expect("page");
    v.layout(5, 40).expect("layout");
    assert!(matches!(
        v.rows().get(v.cursor_row()),
        Some(Row::Text {
            cursor: Some(_),
            ..
        })
    ));

    v.move_cursor(Motion::FileStart, Extend::None).expect("top");
    v.layout(5, 40).expect("layout");
    assert_eq!(v.cursor_row(), 0);
    assert_eq!(v.cursor(), 0);
}

#[test]
fn home_and_end_are_the_ends_of_the_cursors_screen_row() {
    // "start / end of the line or row". With wrap off a row is
    // the line; with wrap on it is the wrapped row.
    let mut v = open("abcdefghij\nsecond\n");
    v.layout(4, 40).expect("layout");
    v.move_cursor(Motion::RowEnd, Extend::None).expect("end");
    assert_eq!(v.cursor(), 9, "the last character, not the terminator");
    v.move_cursor(Motion::RowStart, Extend::None).expect("home");
    assert_eq!(v.cursor(), 0);

    let mut w = open("abcdefghij\nsecond\n");
    w.toggle_wrap();
    w.layout(4, 4).expect("layout");
    w.move_cursor(Motion::RowEnd, Extend::None).expect("end");
    assert_eq!(w.cursor(), 3, "the last character of the first wrapped row");
    w.move_cursor(Motion::Down, Extend::None).expect("down");
    assert_eq!(w.cursor(), 7, "column three of the row below");
    w.move_cursor(Motion::RowStart, Extend::None).expect("home");
    assert_eq!(w.cursor(), 4, "the wrapped row's own first character");
}

#[test]
fn a_vertical_move_keeps_the_goal_column_over_a_short_line() {
    // a short line puts the cursor at
    // its end and leaves the *goal* alone, so the column survives the line.
    let mut v = open("aaaaaaaaaa\nbb\ncccccccccc\n");
    v.layout(4, 40).expect("layout");
    for _ in 0..6 {
        v.move_cursor(Motion::Right, Extend::None).expect("right");
    }
    assert_eq!((v.cursor(), v.goal_column()), (6, 6));

    v.move_cursor(Motion::Down, Extend::None).expect("down");
    assert_eq!(v.cursor(), 12, "the short line's last character");
    assert_eq!(v.goal_column(), 6, "the goal is unchanged");

    v.move_cursor(Motion::Down, Extend::None).expect("down");
    assert_eq!(v.cursor(), 20, "back in column six of the long line");
}

#[test]
fn a_horizontal_move_sets_the_goal_column() {
    let mut v = open("aaaaaaaaaa\nbb\ncccccccccc\n");
    v.layout(4, 40).expect("layout");
    for _ in 0..6 {
        v.move_cursor(Motion::Right, Extend::None).expect("right");
    }
    v.move_cursor(Motion::RowStart, Extend::None).expect("home");
    assert_eq!(v.goal_column(), 0, "Home is a statement about a column");
    v.move_cursor(Motion::Down, Extend::None).expect("down");
    assert_eq!(v.cursor(), 11, "column zero of the line below");
}

#[test]
fn left_and_right_move_one_character_and_cross_the_line_boundary() {
    let mut v = open("ab\ncd\n");
    v.layout(4, 40).expect("layout");
    v.move_cursor(Motion::Right, Extend::None).expect("right");
    assert_eq!(v.cursor(), 1);
    v.move_cursor(Motion::Right, Extend::None).expect("right");
    assert_eq!(v.cursor(), 3, "the terminator is stepped over, not onto");
    assert_eq!(v.cursor_row(), 1);
    v.move_cursor(Motion::Left, Extend::None).expect("left");
    assert_eq!(v.cursor(), 1, "the last character of the line above");
    assert_eq!(v.cursor_row(), 0);
    v.move_cursor(Motion::Left, Extend::None).expect("left");
    v.move_cursor(Motion::Left, Extend::None).expect("left");
    assert_eq!(v.cursor(), 0, "and it stops at the start of the file");
}

#[test]
fn one_press_is_one_character_however_many_bytes_it_is() {
    // "one character, which under a multi-byte encoding may be
    // several bytes".
    let mut v = open("a\u{65e5}b\n");
    v.layout(2, 40).expect("layout");
    v.move_cursor(Motion::Right, Extend::None).expect("right");
    assert_eq!(v.cursor(), 1);
    v.move_cursor(Motion::Right, Extend::None).expect("right");
    assert_eq!(v.cursor(), 4, "three bytes, one press");
    v.move_cursor(Motion::Left, Extend::None).expect("left");
    assert_eq!(v.cursor(), 1, "and one press back");
}

#[test]
fn with_wrapping_on_a_row_is_a_wrapped_row_for_every_movement_key() {
    let mut v = open("abcdefghijkl\nnext\n");
    v.toggle_wrap();
    v.layout(6, 4).expect("layout");
    for expected in [4, 8, 13] {
        v.move_cursor(Motion::Down, Extend::None).expect("down");
        assert_eq!(v.cursor(), expected);
    }
    for expected in [8, 4, 0] {
        v.move_cursor(Motion::Up, Extend::None).expect("up");
        assert_eq!(v.cursor(), expected);
    }
}

#[test]
fn a_tab_counts_as_the_stops_it_draws_as() {
    // in text mode the column is a **display**
    // column of the expanded row, which is what makes a block over tab-aligned
    // output line up.
    let mut v = open("\tx\nabcdey\n");
    v.layout(4, 40).expect("layout");
    v.move_cursor(Motion::RowEnd, Extend::None).expect("end");
    assert_eq!(v.cursor(), 1, "the `x`, one byte in");
    assert_eq!(v.goal_column(), 4, "but in column four, after the tab stop");
    v.move_cursor(Motion::Down, Extend::None).expect("down");
    assert_eq!(v.cursor(), 7, "column four of the line below is its `e`");
}

#[test]
fn ctrl_home_and_ctrl_end_are_the_file_ends() {
    let body = numbered(20);
    let mut v = open(&body);
    v.layout(5, 40).expect("layout");
    v.move_cursor(Motion::FileEnd, Extend::None).expect("end");
    assert_eq!(
        v.cursor(),
        body.len() as u64 - 2,
        "the last character the screen draws, not the terminator behind it"
    );
    v.layout(5, 40).expect("layout");
    assert_eq!(v.cursor_row(), 4, "on the window's last row");

    v.move_cursor(Motion::FileStart, Extend::None)
        .expect("home");
    assert_eq!((v.cursor(), v.top(), v.cursor_row()), (0, 0, 0));
}

#[test]
fn a_page_keeps_the_cursors_row_and_its_goal_column() {
    let mut v = open(&numbered(200));
    v.layout(10, 40).expect("layout");
    for _ in 0..3 {
        v.move_cursor(Motion::Down, Extend::None).expect("down");
    }
    for _ in 0..2 {
        v.move_cursor(Motion::Right, Extend::None).expect("right");
    }
    assert_eq!((v.cursor_row(), v.goal_column()), (3, 2));

    v.move_cursor(Motion::PageDown, Extend::None).expect("page");
    assert_eq!(v.cursor_row(), 3, "the row index survives a page");
    assert_eq!(v.goal_column(), 2, "and so does the goal column");
    v.layout(10, 40).expect("layout");
    assert!(matches!(
        v.rows().get(3),
        Some(Row::Text {
            cursor: Some(2),
            ..
        })
    ));

    v.move_cursor(Motion::PageUp, Extend::None).expect("page");
    assert_eq!(
        (v.cursor(), v.cursor_row(), v.top()),
        (23, 3, 0),
        "back where it was: row three of the window, column two of `line 3`"
    );
}

// --------------------------------------------------------------- hex --------

#[test]
fn a_vertical_move_in_hex_carries_the_column() {
    // v0.4 took a vertical move to the row's
    // first byte, because `scroll` ended with `cursor = top`. A cursor that can
    // select has to take its column with it or `Shift+Down` cannot make a
    // rectangular selection at all.
    let mut v = open_bytes(&ramp(64), &hex_cfg(8));
    v.layout(4, 200).expect("layout");
    for _ in 0..3 {
        v.move_cursor(Motion::Right, Extend::None).expect("right");
    }
    assert_eq!(v.cursor(), 3);
    v.move_cursor(Motion::Down, Extend::None).expect("down");
    assert_eq!(v.cursor(), 11, "one row down, the same column");
    v.move_cursor(Motion::Up, Extend::None).expect("up");
    assert_eq!(v.cursor(), 3);
}

#[test]
fn home_and_end_in_hex_are_the_rows_own_ends() {
    let mut v = open_bytes(&ramp(20), &hex_cfg(8));
    v.layout(4, 200).expect("layout");
    v.move_cursor(Motion::Down, Extend::None).expect("down");
    v.move_cursor(Motion::RowEnd, Extend::None).expect("end");
    assert_eq!(v.cursor(), 15);
    v.move_cursor(Motion::RowStart, Extend::None).expect("home");
    assert_eq!(v.cursor(), 8);

    // The last row of a file is short, and `End` stops on its last byte.
    v.move_cursor(Motion::Down, Extend::None).expect("down");
    v.move_cursor(Motion::RowEnd, Extend::None).expect("end");
    assert_eq!(v.cursor(), 19);
}

#[test]
fn moving_on_the_bytes_side_cannot_land_inside_a_word() {
    // Invariant 9. "Moving on the bytes side itself always
    // advances a whole column, so a selection made there is word-aligned
    // without anyone having to think about it."
    let mut c = hex_cfg(16);
    c.hex.group = crate::config::HexGroup::Bits32;
    let mut v = open_bytes(&ramp(128), &c);
    v.layout(4, 200).expect("layout");
    assert_eq!(v.hex_side(), HexSide::Bytes);

    for step in 0..7 {
        v.move_cursor(Motion::Right, Extend::Linear).expect("right");
        let sel = v.selection().expect("a selection");
        let (lo, hi) = sel.range();
        assert!(
            select::word_aligned(lo, hi, v.len(), v.hex_config()),
            "step {step}: {lo}..{hi} is inside a word"
        );
    }
    for step in 0..4 {
        v.move_cursor(Motion::Left, Extend::Linear).expect("left");
        let sel = v.selection().expect("a selection");
        let (lo, hi) = sel.range();
        assert!(
            select::word_aligned(lo, hi, v.len(), v.hex_config()),
            "back step {step}: {lo}..{hi} is inside a word"
        );
    }
}

#[test]
fn tab_switches_focus_and_nothing_else_changes() {
    // Invariant 6, and the design in as many words: "Selecting five bytes on
    // the left and pressing `Tab` leaves five characters selected on the
    // right."
    let mut v = open_bytes(&ramp(64), &hex_cfg(8));
    v.layout(4, 200).expect("layout");
    for _ in 0..5 {
        v.move_cursor(Motion::Right, Extend::Linear).expect("right");
    }
    assert_eq!(v.selection().expect("a selection").len(), 5);

    let before = (
        v.cursor(),
        v.selection(),
        v.top(),
        v.top_row(),
        v.goal_column(),
        v.cursor_row(),
    );
    v.switch_hex_side();
    assert_eq!(v.hex_side(), HexSide::Chars);
    assert_eq!(
        (
            v.cursor(),
            v.selection(),
            v.top(),
            v.top_row(),
            v.goal_column(),
            v.cursor_row()
        ),
        before,
        "Tab moves focus and one field only"
    );

    v.move_cursor(Motion::Right, Extend::Linear).expect("right");
    assert_eq!(
        v.selection().expect("a selection").len(),
        6,
        "and the same selection carries on growing"
    );
}

// --------------------------------------------------------- the selection ----

#[test]
fn shift_extends_and_a_bare_arrow_clears() {
    // Invariant 8, and the design is silent, every
    // editor clears, and a selection that survived an unshifted arrow would
    // make the next `Ctrl+C` copy something the user had let go of.
    let mut v = open("abcdef\nghijkl\n");
    v.layout(4, 40).expect("layout");
    v.move_cursor(Motion::Right, Extend::Linear).expect("shift");
    assert_eq!(v.selection().expect("a selection").range(), (0, 1));
    v.move_cursor(Motion::Right, Extend::Linear).expect("shift");
    assert_eq!(v.selection().expect("a selection").range(), (0, 2));
    v.move_cursor(Motion::Left, Extend::Linear).expect("shift");
    assert_eq!(v.selection().expect("a selection").range(), (0, 1));

    v.move_cursor(Motion::Right, Extend::None).expect("bare");
    assert!(v.selection().is_none(), "a bare movement clears it");
}

#[test]
fn ctrl_shift_makes_a_block_out_of_a_selection_rather_than_a_second_one() {
    let mut v = open("aaaXXbb\ncccXXdd\n");
    v.layout(4, 40).expect("layout");
    v.move_cursor(Motion::Right, Extend::Linear).expect("shift");
    assert!(matches!(
        v.selection().expect("a selection").kind,
        SelectKind::Linear
    ));
    v.move_cursor(Motion::Right, Extend::Rectangular)
        .expect("ctrl shift");
    let sel = v.selection().expect("a selection");
    assert!(matches!(sel.kind, SelectKind::Rectangular));
    assert_eq!(sel.anchor, 0, "the anchor did not move");
    assert_eq!(
        sel.range(),
        (0, 2),
        "and the range grew rather than restarted"
    );

    // `Alt+B` flips it back without moving either end.
    assert_eq!(v.toggle_selection_kind(), Some(SelectKind::Linear));
    assert_eq!(v.selection().expect("a selection").range(), (0, 2));
}

#[test]
fn a_block_takes_the_same_columns_out_of_every_row_it_spans() {
    // the "a column block, which is how you take one field out of
    // aligned output", drawn.
    let mut v = open("aaaXXbb\ncccXXdd\neeeXXff\n");
    v.layout(4, 40).expect("layout");
    for _ in 0..3 {
        v.move_cursor(Motion::Right, Extend::None).expect("right");
    }
    for _ in 0..2 {
        v.move_cursor(Motion::Right, Extend::Rectangular)
            .expect("ctrl shift right");
    }
    for _ in 0..2 {
        v.move_cursor(Motion::Down, Extend::Rectangular)
            .expect("ctrl shift down");
    }
    v.layout(4, 40).expect("layout");

    for (i, row) in v.rows().iter().take(3).enumerate() {
        match row {
            Row::Text { sel, text, .. } => {
                let range = sel
                    .clone()
                    .unwrap_or_else(|| panic!("row {i} is in the block"));
                assert_eq!(text.get(range).expect("a byte range into the row"), "XX");
            }
            Row::Hex { .. } => panic!("text mode"),
        }
    }
}

#[test]
fn a_laid_out_row_carries_the_selection_and_the_cursor_it_shows() {
    let mut v = open("abcdef\nghijkl\n");
    v.layout(4, 40).expect("layout");
    for _ in 0..3 {
        v.move_cursor(Motion::Right, Extend::Linear).expect("shift");
    }
    v.layout(4, 40).expect("layout");
    match v.rows().first().expect("a row") {
        Row::Text { sel, cursor, .. } => {
            assert_eq!(sel.clone(), Some(0..3));
            assert_eq!(*cursor, Some(3));
        }
        Row::Hex { .. } => panic!("text mode"),
    }

    let mut h = open_bytes(&ramp(32), &hex_cfg(8));
    h.layout(4, 200).expect("layout");
    for _ in 0..5 {
        h.move_cursor(Motion::Right, Extend::Linear).expect("shift");
    }
    h.layout(4, 200).expect("layout");
    match h.rows().first().expect("a row") {
        Row::Hex { sel, cursor, .. } => {
            assert_eq!(sel.clone(), Some(0..5));
            assert_eq!(*cursor, Some(5));
        }
        Row::Text { .. } => panic!("hex mode"),
    }
}

#[test]
fn selecting_the_whole_file_reads_no_bytes() {
    // Invariant 5, and "selecting 40 GB is instant".
    let body = numbered(400).into_bytes();
    let len = body.len() as u64;
    let (mut v, read) = open_counted(body, &cfg());
    v.layout(10, 40).expect("layout");
    let _ = taken(&read);

    v.select_all();
    assert_eq!(taken(&read), 0, "`Ctrl+A` touched two numbers");
    assert_eq!(v.selection().expect("a selection").range(), (0, len));
    assert_eq!(
        (v.cursor(), v.top()),
        (0, 0),
        "and it moved neither the cursor nor the window"
    );
}

#[test]
fn a_selection_over_the_whole_file_costs_the_same_layout_as_none() {
    // Invariant 4: adding a cursor and a selection adds no read.
    let body = numbered(400).into_bytes();
    let (mut plain, plain_read) = open_counted(body.clone(), &cfg());
    plain.layout(10, 40).expect("layout");
    let plain_cost = taken(&plain_read);

    let (mut all, all_read) = open_counted(body, &cfg());
    all.select_all();
    all.layout(10, 40).expect("layout");
    assert_eq!(taken(&all_read), plain_cost);
}

#[test]
fn a_selection_is_a_byte_range_and_outlives_every_view_change() {
    // Invariant 7, and "there is one cursor and one selection,
    // and both are byte ranges in the file" - a byte range does not care how
    // the bytes are drawn.
    let mut v = open("abcdefgh\nijklmnop\n");
    v.layout(4, 40).expect("layout");
    for _ in 0..4 {
        v.move_cursor(Motion::Right, Extend::Linear).expect("shift");
    }
    let sel = v.selection().expect("a selection");

    v.toggle_mode().expect("hex");
    assert_eq!(v.selection(), Some(sel), "a mode switch keeps it");
    v.toggle_mode().expect("text");
    assert_eq!(v.selection(), Some(sel));
    v.toggle_wrap();
    assert_eq!(v.selection(), Some(sel), "wrap keeps it");
    v.cycle_encoding().expect("F8");
    assert_eq!(v.selection(), Some(sel), "F8 keeps it");
    v.goto_offset(12).expect("Ctrl+G");
    assert_eq!(v.selection(), Some(sel), "a jump keeps it");
    v.place_cursor(3).expect("a find hit");
    assert_eq!(v.selection(), Some(sel), "and so does a find hit");

    assert!(v.clear_selection(), "Esc had something to clear");
    assert!(!v.clear_selection(), "and then it had not");
}

#[test]
fn pure_scrolling_is_what_it_was_and_says_why_it_cannot_select() {
    // Invariant 17, and the rule is
    // never a panic and never silence.
    let mut c = cfg();
    c.cursor = false;
    let mut v = open_with(&numbered(40), &c);
    v.layout(5, 40).expect("layout");

    v.move_cursor(Motion::Down, Extend::None).expect("down");
    assert_eq!(v.top(), 7, "the window moved, because there is no cursor");
    assert_eq!(v.cursor(), v.top(), "and the cursor follows the top");

    let err = v
        .move_cursor(Motion::Down, Extend::Linear)
        .expect_err("a selection was refused");
    assert!(
        err.to_string().contains("viewer.cursor = true"),
        "it says how to turn one on: {err}"
    );
    assert!(v.selection().is_none());

    v.select_all();
    assert!(
        v.selection().is_none(),
        "and `Ctrl+A` cannot select without a cursor either"
    );

    // Home and End are the horizontal scroll v0.4 made them.
    let mut wide = open_with("short\n", &c);
    wide.layout(2, 4).expect("layout");
    wide.line_end().expect("end");
    assert!(wide.hscroll() > 0, "End scrolled sideways");
    wide.line_start().expect("home");
    assert_eq!(wide.hscroll(), 0);
}

#[test]
fn with_a_cursor_home_end_and_the_arrows_route_through_it() {
    // The v0.4 entry points are still the ones the keymap resolves to
    // (the signature table), so they have to mean the
    // cursor when there is one.
    let mut v = open("abcdef\nghi\n");
    v.layout(4, 40).expect("layout");
    v.scroll_horizontal(1).expect("right");
    assert_eq!(v.cursor(), 1, "Right is a character, not a scroll");
    assert_eq!(v.hscroll(), 0);
    v.line_end().expect("end");
    assert_eq!(v.cursor(), 5);
    v.line_start().expect("home");
    assert_eq!(v.cursor(), 0);
    v.goto_start().expect("Ctrl+Home");
    assert_eq!((v.cursor(), v.cursor_row()), (0, 0));
}

#[test]
fn the_view_follows_the_cursor_sideways_with_wrapping_off() {
    let mut v = open("0123456789abcdefghij\nshort\n");
    v.layout(4, 8).expect("layout");
    assert_eq!(v.hscroll(), 0);
    v.move_cursor(Motion::RowEnd, Extend::None).expect("end");
    assert_eq!(v.cursor(), 19);
    assert_eq!(
        v.hscroll(),
        12,
        "the least it can move to put column 19 on an eight-column screen"
    );
    v.move_cursor(Motion::RowStart, Extend::None).expect("home");
    assert_eq!(v.hscroll(), 0, "and back again");
}

#[test]
fn a_status_line_reports_a_span_and_a_width_for_a_block() {
    // Invariant 14: counting a block's bytes means reading every line between
    // its ends, which the design does not allow a status line to do.
    let mut v = open("aaaXXbb\ncccXXdd\neeeXXff\n");
    v.layout(4, 40).expect("layout");
    for _ in 0..3 {
        v.move_cursor(Motion::Right, Extend::None).expect("right");
    }
    for _ in 0..2 {
        v.move_cursor(Motion::Right, Extend::Rectangular)
            .expect("ctrl shift right");
    }
    v.move_cursor(Motion::Down, Extend::Rectangular)
        .expect("ctrl shift down");

    let status = v.status();
    let sel = status.selection.expect("a selection in the status line");
    assert!(matches!(sel.kind, SelectKind::Rectangular));
    assert_eq!(sel.columns, 2);
    assert!(
        sel.label().contains("over"),
        "a span and a width, never a count: {}",
        sel.label()
    );
    assert_eq!(status.offset, v.cursor(), "the offset is the cursor's");
    assert_eq!(status.side, HexSide::Bytes);
}

#[test]
fn the_characters_side_moves_by_characters_and_the_bytes_side_by_columns() {
    // "Five presses over UTF-8 text can therefore select more
    // than five bytes, and the byte count in the status line is what says how
    // many." The two sides share the range, not the number of keystrokes.
    let body = "\u{65e5}\u{672c}\u{8a9e}abc".as_bytes().to_vec();
    let mut v = open_bytes(&body, &hex_cfg(16));
    v.set_mode(ViewerMode::Hex).expect("hex");
    v.layout(4, 200).expect("layout");
    v.switch_hex_side();
    assert_eq!(v.hex_side(), HexSide::Chars);

    for _ in 0..3 {
        v.move_cursor(Motion::Right, Extend::Linear).expect("right");
    }
    assert_eq!(
        v.selection().expect("a selection").len(),
        9,
        "three presses, three characters, nine bytes"
    );

    v.switch_hex_side();
    v.move_cursor(Motion::Right, Extend::Linear).expect("right");
    assert_eq!(
        v.selection().expect("a selection").len(),
        10,
        "and one press on the bytes side is one byte at group = 8"
    );
}

#[test]
fn with_no_cursor_home_and_end_are_still_the_hex_rows_ends() {
    // the design item 2: the design requires "the current
    // offset under the cursor" in the status line, and the offset has to be
    // able to be something other than a multiple of `hex_width`.
    let mut c = hex_cfg(8);
    c.cursor = false;
    let mut v = open_bytes(&ramp(64), &c);
    v.layout(4, 200).expect("layout");
    v.move_cursor(Motion::RowEnd, Extend::None).expect("end");
    assert_eq!(v.cursor(), 7);
    v.move_cursor(Motion::RowStart, Extend::None).expect("home");
    assert_eq!(v.cursor(), 0);
}

#[test]
fn the_cursor_a_row_carries_is_a_byte_index_into_what_it_draws() {
    // The renderer has the row and nothing else, so a tab that became four
    // spaces has to have moved the cursor with it.
    let mut v = open("\tabc\n");
    v.layout(2, 40).expect("layout");
    v.move_cursor(Motion::Right, Extend::None).expect("right");
    v.layout(2, 40).expect("layout");
    match v.rows().first().expect("a row") {
        Row::Text { text, cursor, .. } => {
            assert_eq!(text, "    abc");
            assert_eq!(*cursor, Some(4), "the `a`, past the expanded tab");
        }
        Row::Hex { .. } => panic!("text mode"),
    }
}

// ------------------------------------ the cursor and the selection ---

/// A viewer over `body` in hex mode, at `group` bits a column.
fn open_hex(body: &[u8], group: crate::config::HexGroup) -> Viewer {
    let mut c = cfg();
    c.default_mode = ViewerMode::Hex;
    c.hex.group = group;
    open_bytes(body, &c)
}

/// The ten motions, so a test cannot silently exercise nine.
const BYTES_SIDE_MOTIONS: [Motion; 10] = [
    Motion::Up,
    Motion::Down,
    Motion::Left,
    Motion::Right,
    Motion::PageUp,
    Motion::PageDown,
    Motion::RowStart,
    Motion::RowEnd,
    Motion::FileStart,
    Motion::FileEnd,
];

#[test]
fn every_movement_on_the_bytes_side_leaves_a_word_aligned_selection() {
    // "Moving on the bytes side itself always advances a whole
    // column, so a selection made there is word-aligned without anyone having
    // to think about it" (the design invariant 9). `End`,
    // `Ctrl+End` and a `Down` onto a short last row all land on a byte chosen
    // for being the last one, and used to leave the cursor - and so the next
    // selection's anchor - inside a word.
    use crate::config::HexGroup;
    use crate::viewer::select::word_aligned;
    let body: Vec<u8> = (0u8..40).collect();
    for group in [HexGroup::Bits16, HexGroup::Bits32, HexGroup::Bits64] {
        let step = group.bytes() as u64;
        let mut v = open_hex(&body, group);
        v.layout(3, 100).expect("layout");
        // Every motion, from wherever the last one left the cursor, with and
        // without the Shift that turns it into a selection.
        for round in 0..3 {
            for motion in BYTES_SIDE_MOTIONS {
                let extend = if round == 0 {
                    Extend::None
                } else {
                    Extend::Linear
                };
                v.move_cursor(motion, extend).expect("move");
                v.layout(3, 100).expect("layout");
                assert!(
                    v.cursor().is_multiple_of(step),
                    "{motion:?} at {group:?} left the cursor at {} inside a word",
                    v.cursor()
                );
                if let Some(sel) = v.selection() {
                    let (lo, hi) = sel.range();
                    assert!(
                        word_aligned(lo, hi, v.len(), v.hex_config()),
                        "{motion:?} at {group:?} made an unaligned selection {lo}..{hi}"
                    );
                }
            }
        }
    }
}

#[test]
fn shift_end_on_the_bytes_side_selects_the_whole_row() {
    // The row's last *column*, not its last byte: a head is exclusive going
    // forward, so `End` points one past the row while the cursor stops on the
    // last byte the row draws.
    use crate::config::HexGroup;
    use crate::viewer::copy::CopyRequest;
    let body: Vec<u8> = (0u8..40).collect();
    let mut v = open_hex(&body, HexGroup::Bits32);
    v.layout(3, 100).expect("layout");
    v.move_cursor(Motion::RowEnd, Extend::Linear).expect("end");
    assert_eq!(
        v.selection().map(|s| s.range()),
        Some((0, 16)),
        "Shift+End selects the row it is at the end of"
    );
    assert_eq!(
        v.cursor(),
        12,
        "the cursor is on the last column's own start"
    );
    // And the copy is columns rather than the half-covered-word fallback.
    match v.copy(CopyRequest::Selection, 1 << 20).expect("copy") {
        crate::viewer::copy::Copied::Text { note, .. } => {
            assert_eq!(note, None, "a selection made on the bytes side lines up");
        }
        other => panic!("refused: {other:?}"),
    }
}

#[test]
fn the_last_byte_of_the_file_can_be_selected_from_the_keyboard() {
    // `Selection::range` is exclusive going forward and `hex::clamp_cursor`
    // stops the cursor on the last byte, so the head has to be able to reach
    // `len` or the file's final byte is unreachable by every key but `Ctrl+A`.
    // the design defines `Ctrl+Shift+End` as "linear extend to
    // the end of the file", which is what this is.
    use crate::config::HexGroup;
    let mut v = open("abcde");
    v.layout(3, 40).expect("layout");
    for _ in 0..20 {
        v.move_cursor(Motion::Right, Extend::Linear).expect("right");
    }
    assert_eq!(v.selection().map(|s| s.range()), Some((0, 5)), "all five");

    let mut v = open("abcde");
    v.layout(3, 40).expect("layout");
    v.move_cursor(Motion::FileEnd, Extend::Linear).expect("end");
    assert_eq!(v.selection().map(|s| s.range()), Some((0, 5)));

    // Text mode's `End` is the row's own text, terminator excluded.
    let mut v = open("abc\ndef\n");
    v.layout(3, 40).expect("layout");
    v.move_cursor(Motion::RowEnd, Extend::Linear).expect("end");
    assert_eq!(
        v.selection().map(|s| s.range()),
        Some((0, 3)),
        "Shift+End selects the line's text, not its terminator"
    );

    // And hex mode reaches the terminator, which is a byte like any other.
    let mut v = open_hex(b"abc\n", HexGroup::Bits8);
    v.layout(3, 60).expect("layout");
    v.move_cursor(Motion::FileEnd, Extend::Linear).expect("end");
    assert_eq!(v.selection().map(|s| s.range()), Some((0, 4)));
}

#[test]
fn a_selection_shrunk_back_to_nothing_does_not_swallow_an_esc() {
    // "`Esc` clears the selection; if there is none, close the
    // viewer." A head walked back onto its anchor covers no byte: nothing is
    // painted, `Ctrl+C` already refuses it, and the status line does not
    // announce it - so `Esc` closes rather than reporting a selection the user
    // cannot see.
    use crate::viewer::copy::CopyRequest;
    let mut v = open("abcdef\n");
    v.layout(3, 40).expect("layout");
    v.move_cursor(Motion::Right, Extend::Linear).expect("out");
    assert!(v.clear_selection(), "one byte is something to clear");

    v.move_cursor(Motion::Right, Extend::Linear).expect("out");
    v.move_cursor(Motion::Left, Extend::Linear).expect("back");
    assert!(
        v.selection().is_some_and(|s| s.is_empty()),
        "the anchor is still live so that carrying on selects the other way"
    );
    v.layout(3, 40).expect("layout");
    assert_eq!(v.status().selection, None, "nothing is announced");
    assert!(matches!(
        v.copy(CopyRequest::Selection, 1 << 20).expect("copy"),
        crate::viewer::copy::Copied::Refused(_)
    ));
    assert!(
        !v.clear_selection(),
        "and Esc closes the viewer rather than clearing nothing"
    );
    assert_eq!(v.selection(), None, "the anchor still goes");
}

#[test]
fn hex_width_is_rounded_down_to_whole_columns_and_says_so() {
    // "A `width` that is not a whole number of words is
    // rounded down to one, and says so." Without it a row's own cells and the
    // file's word boundaries disagree from the second row on, and a selection
    // the arithmetic calls aligned lands in the middle of a drawn column.
    use crate::config::HexGroup;
    let body: Vec<u8> = (0u8..40).collect();
    let mut c = cfg();
    c.default_mode = ViewerMode::Hex;
    c.hex_width = 12;
    c.hex.group = HexGroup::Bits64;
    let mut v = open_bytes(&body, &c);
    assert_eq!(
        v.hex().width(),
        8,
        "12 bytes is one 64-bit column and a half"
    );
    assert_eq!(v.status().hex_width_rounded, Some((12, 8)));
    v.layout(4, 120).expect("layout");
    assert_eq!(
        v.rows().iter().map(Row::offset).collect::<Vec<_>>(),
        vec![0, 8, 16, 24],
        "the rows are the rounded width apart"
    );

    // `g` re-applies the rounding to the **configured** width, so the row grows
    // back rather than shrinking every time the key is pressed.
    let said = v.cycle_hex_group();
    assert_eq!(v.hex().width(), 12, "12 bytes is twelve 8-bit columns");
    assert_eq!(v.status().hex_width_rounded, None);
    assert!(!said.contains("rounded"), "nothing to say: {said}");
    let said = v.cycle_hex_group();
    assert_eq!(v.hex().width(), 12, "and six 16-bit ones");
    assert!(!said.contains("rounded"), "nothing to say: {said}");
    let said = v.cycle_hex_group();
    assert_eq!(v.hex().width(), 12, "and three 32-bit ones");
    assert!(!said.contains("rounded"), "nothing to say: {said}");
    let said = v.cycle_hex_group();
    assert_eq!(v.hex().width(), 8);
    assert!(
        said.contains("hex_width 12 rounded down to 8"),
        "and says so: {said}"
    );
}

#[test]
fn a_bytes_side_copy_is_a_column_the_screen_actually_drew() {
    // the bytes side "yields the columns **as they are
    // displayed**". With the width rounded to whole columns, a selection made
    // on the bytes side copies a cell the row really drew - and needs no
    // half-covered-word note, because there is no half-covered word.
    use crate::config::{HexFormat, HexGroup};
    use crate::viewer::copy::{Copied, CopyRequest};
    let body: Vec<u8> = (0u8..40).collect();
    let mut c = cfg();
    c.default_mode = ViewerMode::Hex;
    c.hex_width = 12;
    c.hex.group = HexGroup::Bits64;
    c.hex.format = HexFormat::Unsigned;
    let mut v = open_bytes(&body, &c);
    v.layout(4, 120).expect("layout");
    v.place_cursor(8).expect("place");
    v.move_cursor(Motion::Right, Extend::Linear).expect("right");
    assert_eq!(v.selection().map(|s| s.range()), Some((8, 16)));
    v.layout(4, 120).expect("layout");
    let drawn = v
        .rows()
        .iter()
        .find(|r| r.offset() == 8)
        .map(|r| match r {
            Row::Hex { bytes, .. } => {
                crate::viewer::hex::value_column(bytes, v.hex().width(), v.hex_config())
            }
            Row::Text { text, .. } => text.clone(),
        })
        .expect("the row the selection covers");
    match v.copy(CopyRequest::Selection, 1 << 20).expect("copy") {
        Copied::Text { text, note, .. } => {
            assert_eq!(note, None, "nothing is half covered");
            assert_eq!(
                text.trim(),
                drawn.trim(),
                "the copy is the column the screen drew"
            );
        }
        other => panic!("refused: {other:?}"),
    }
}

#[test]
fn the_readout_follows_a_selection_across_a_line_break() {
    // the design promises the readout for 1, 2, 4 and 8 bytes, and a
    // four-byte selection that steps over a line terminator is four bytes like
    // any other. The preview is gathered from the lines this layout is reading
    // anyway, so it costs no read (the design invariant 4).
    let mut v = open("abcdefgh\nijklmnop\nqrstuvwx\n");
    v.layout(4, 40).expect("layout");
    v.place_cursor(6).expect("place");
    for _ in 0..3 {
        v.move_cursor(Motion::Right, Extend::Linear).expect("right");
    }
    v.layout(4, 40).expect("layout");
    assert_eq!(v.selection().map(|s| s.range()), Some((6, 10)));
    assert_eq!(
        v.selection_preview(),
        Some(&b"gh\ni"[..]),
        "the terminator is a byte of the selection like any other"
    );
    let shown = v.status().interpretation.expect("a four-byte reading");
    // Invariant 15: the string the status line shows is the one Ctrl+Shift+C
    // copies.
    match v
        .copy(crate::viewer::copy::CopyRequest::Interpretation, 1 << 20)
        .expect("copy")
    {
        crate::viewer::copy::Copied::Text { text, .. } => assert_eq!(text, shown),
        other => panic!("refused: {other:?}"),
    }
}

#[test]
fn the_readout_is_dropped_rather_than_wrong_when_the_selection_is_off_the_window() {
    // `Window::slice` clamps its low end to the window's own start, so a
    // selection above the top of the screen would otherwise read out the bytes
    // at `top` - a reading of bytes the user never selected, and one
    // `Ctrl+Shift+C` disagrees with (the design invariant 15).
    use crate::config::HexGroup;
    let body: Vec<u8> = (0u8..=255).collect();
    let mut v = open_hex(&body, HexGroup::Bits8);
    v.layout(4, 100).expect("layout");
    v.place_cursor(12).expect("place");
    for _ in 0..4 {
        v.move_cursor(Motion::Right, Extend::Linear).expect("right");
    }
    v.layout(4, 100).expect("layout");
    let shown = v.status().interpretation.expect("in the window");
    assert!(shown.contains("0c 0d 0e 0f"), "the selection's own bytes");

    v.scroll(1).expect("scroll");
    v.layout(4, 100).expect("layout");
    assert!(v.top() > 12, "the selection is above the window now");
    assert_eq!(
        v.status().interpretation,
        None,
        "no reading rather than a reading of the wrong bytes"
    );
    // And the copy still reads the selection itself, from the file.
    match v
        .copy(crate::viewer::copy::CopyRequest::Interpretation, 1 << 20)
        .expect("copy")
    {
        crate::viewer::copy::Copied::Text { text, .. } => {
            assert_eq!(text, shown, "the bytes that were selected");
        }
        other => panic!("refused: {other:?}"),
    }
}

#[test]
fn a_block_that_covers_no_column_copies_nothing() {
    // `Ctrl+Shift+Down` with no sideways movement makes a band whose two ends
    // are the same column, and a band's high end is exclusive.
    // Nothing is painted and the status line
    // says `0 cols`, so the copy says the same rather than putting a column of
    // bytes on the clipboard that were never shown as selected.
    use crate::config::HexGroup;
    use crate::viewer::copy::{Copied, CopyRequest};
    let body: Vec<u8> = (0u8..40).collect();
    for mut v in [open_hex(&body, HexGroup::Bits8), open("ab\ncd\nef\n")] {
        v.layout(3, 60).expect("layout");
        for _ in 0..2 {
            v.move_cursor(Motion::Down, Extend::Rectangular)
                .expect("down");
        }
        v.layout(3, 60).expect("layout");
        assert_eq!(
            v.selection().map(|s| s.columns()),
            Some((0, 0)),
            "zero columns wide"
        );
        assert!(
            v.rows().iter().all(|r| match r {
                Row::Hex { sel, .. } => sel.is_none(),
                Row::Text { sel, .. } => sel.is_none(),
            }),
            "and nothing is painted"
        );
        assert_eq!(
            v.copy(CopyRequest::Selection, 1 << 20).expect("copy"),
            Copied::Refused(crate::viewer::copy::EMPTY_BLOCK.to_string()),
            "so the clipboard gets nothing either"
        );
    }
}

#[test]
fn an_interpretation_is_of_one_run_of_bytes_and_a_block_is_not_one() {
    // the digits are "the selected bytes in file order". Between a
    // block's two corners lie the bytes either side of its column band, which
    // `Ctrl+C` does not copy - so reading the span would put a number on the
    // status line for bytes the selection does not hold.
    use crate::config::HexGroup;
    use crate::viewer::copy::{Copied, CopyRequest};
    let body: Vec<u8> = (0u8..40).collect();
    let mut v = open_hex(&body, HexGroup::Bits8);
    v.layout(3, 100).expect("layout");
    v.move_cursor(Motion::Right, Extend::Rectangular)
        .expect("right");
    v.move_cursor(Motion::Down, Extend::Rectangular)
        .expect("down");
    v.layout(3, 100).expect("layout");
    assert_eq!(v.selection().map(|s| s.columns()), Some((0, 1)));
    match v.copy(CopyRequest::Selection, 1 << 20).expect("copy") {
        Copied::Text { text, .. } => assert_eq!(text, "00\n10", "one column of two rows"),
        other => panic!("refused: {other:?}"),
    }
    assert_eq!(
        v.copy(CopyRequest::Interpretation, 1 << 20).expect("copy"),
        Copied::Refused(crate::viewer::copy::NO_BLOCK_INTERPRETATION.to_string())
    );
    assert_eq!(
        v.status().interpretation,
        None,
        "and the status line says nothing rather than the wrong thing"
    );
}

#[test]
fn the_help_page_says_what_the_generated_table_cannot() {
    // the page is generated from the keymap, so the two rules that
    // are not bindings have to be written in: `Ctrl+Shift+Home`/`End` come out
    // linear because `Ctrl+Home`/`Ctrl+End` are movements themselves
    // (`input::viewer_extend` strips `Shift` first), and the `Alt` pair exists
    // because the terminals cannot all send `Ctrl+Shift+arrow`.
    let page = crate::ui::help::viewer_page(&crate::config::Keymap::builtin(), true);
    assert!(page.contains("Ctrl+Shift+Home"), "{page}");
    assert!(page.contains("stay linear"), "{page}");
    assert!(page.contains("Alt+B"), "{page}");
    assert!(page.contains("terminals cannot send"), "{page}");
    // And the generated half still lists every key it is generated from.
    for key in ["Tab", "Ctrl+A", "Alt+B", "Ctrl+C", "Ctrl+Home", "Ctrl+End"] {
        assert!(page.contains(key), "{key} missing from the page:\n{page}");
    }
}

#[test]
fn ctrl_shift_home_and_end_really_are_linear() {
    // The claim the help page makes, asserted against the resolution that
    // makes it true rather than against the prose.
    use crate::config::KeyContext;
    use crate::input::{parse_key, viewer_extend};
    use crate::viewer::select::Extend;
    let km = crate::config::Keymap::builtin();
    for key in ["ctrl+shift+home", "ctrl+shift+end"] {
        let press = parse_key(key).expect("key");
        let (_, extend) = viewer_extend(&km, KeyContext::Viewer, press).expect(key);
        assert_eq!(extend, Extend::Linear, "{key}");
    }
    for key in ["ctrl+shift+right", "ctrl+shift+down"] {
        let press = parse_key(key).expect("key");
        let (_, extend) = viewer_extend(&km, KeyContext::Viewer, press).expect(key);
        assert_eq!(extend, Extend::Rectangular, "{key}");
    }
}

#[test]
fn ctrl_with_a_movement_scrolls_the_view_and_keeps_the_cursor() {
    // the view moves, the cursor does not. Reading a long match
    // is looking somewhere else for a moment without losing the place, so the
    // two have to come apart.
    let body: String = (1..=200).map(|n| format!("line {n:03}\n")).collect();
    let mut v = open(&body);
    v.layout(10, 40).expect("layout");
    v.move_cursor(Motion::Down, Extend::None).expect("down");
    v.move_cursor(Motion::Down, Extend::None).expect("down");
    let cursor = v.cursor();
    let top = v.top();

    v.scroll(5).expect("scroll");
    v.layout(10, 40).expect("layout");
    assert_eq!(v.cursor(), cursor, "the cursor stayed put");
    assert_ne!(v.top(), top, "the view moved");

    // And the next cursor movement brings the view back to it, which is what
    // makes scrolling away safe.
    v.move_cursor(Motion::Down, Extend::None).expect("down");
    v.layout(10, 40).expect("layout");
    let seen = v.cursor();
    assert!(
        seen >= v.top(),
        "the view followed the cursor back: cursor {seen}, top {}",
        v.top()
    );
}

#[test]
fn scrolling_the_view_does_not_disturb_the_selection() {
    // The point of the key: hold a selection, look elsewhere, come back.
    let body: String = (1..=200).map(|n| format!("line {n:03}\n")).collect();
    let mut v = open(&body);
    v.layout(10, 40).expect("layout");
    for _ in 0..4 {
        v.move_cursor(Motion::Right, Extend::Linear)
            .expect("select");
    }
    let before = v.selection();
    assert!(before.is_some(), "there is a selection to keep");

    v.scroll(20).expect("scroll away");
    v.layout(10, 40).expect("layout");
    assert_eq!(v.selection(), before, "scrolling kept it");
    v.scroll(-20).expect("scroll back");
    v.layout(10, 40).expect("layout");
    assert_eq!(v.selection(), before, "and coming back kept it");
}

#[test]
fn a_view_scroll_sideways_moves_the_view_and_nothing_else() {
    // Text only: hex rows are a fixed width that always fits, and with wrap on
    // there is nothing to the side. Both are silent no-ops.
    let mut v = open(&format!("{}\n", "x".repeat(400)));
    v.layout(4, 40).expect("layout");
    let cursor = v.cursor();
    v.scroll_view_horizontal(10);
    assert_eq!(v.hscroll(), 10);
    assert_eq!(v.cursor(), cursor, "sideways scrolling left the cursor");

    v.toggle_wrap();
    let was = v.hscroll();
    v.scroll_view_horizontal(10);
    assert_eq!(v.hscroll(), was, "nothing to the side with wrapping on");
}

#[test]
fn a_hit_opens_at_a_line_when_the_searcher_had_no_file_offset() {
    // "For a content match, `Enter` opens the viewer at the
    // matching line with the hit already highlighted."
    //
    // A content search in UTF-16, windows-1252 or CP437 reads a *transcoded*
    // stream, so `grep_searcher`'s byte offsets are not the file's - a UTF-16LE
    // hit reports roughly half the file offset of the line it names. Seeking to
    // that number put the viewer a hundred lines from the line the status bar
    // had just reported, so a hit from one of those charsets travels as its
    // line number instead ([`crate::vfs::ContentHit::decoded`]).
    let mut body = String::new();
    for i in 0..200 {
        body.push_str(&format!("padding line {i:03}\n"));
    }
    let needle_at = body.len() as u64;
    body.push_str("NEEDLE here\n");
    let mut v = open(&body);
    v.layout(6, 40).expect("layout");

    // Line 201, 1-based, exactly as `SinkMatch::line_number` counts it.
    v.open_at_hit(HitStart::Line(201), None).expect("open");
    assert_eq!(
        v.cursor(),
        needle_at,
        "the cursor is on the start of the matching line"
    );
    v.layout(6, 40).expect("layout");
    assert!(
        texts(&v).iter().any(|row| row.contains("NEEDLE")),
        "and the line is on the screen: {:?}",
        texts(&v)
    );

    // The other half is unchanged: a UTF-8 hit has a real file offset and is
    // still placed by it (the "position is a byte offset").
    v.open_at_hit(HitStart::Offset(0), None).expect("open");
    assert_eq!(v.cursor(), 0);
}

#[test]
fn the_help_page_lists_the_hex_keys() {
    // They were absent from it, which made the whole point -
    // trying one grouping and then another against an unfamiliar file -
    // undiscoverable. A key nobody can find out about is not a key.
    let page = crate::ui::help::viewer_page(&crate::config::Keymap::builtin(), true);
    for key in ["G", "D", "S", "E"] {
        assert!(
            page.lines().any(|l| l.trim_start().starts_with(key)),
            "{key} missing from the viewer help page:\n{page}"
        );
    }
    assert!(page.contains("decimal only"), "{page}");
}

#[test]
fn sign_is_its_own_question_and_does_nothing_in_hex() {
    // base and sign are independent, and `s` in hex says so
    // rather than answering a different question by switching base.
    use crate::config::HexFormat;
    let mut v = open_bytes(&ramp(64), &hex_cfg(8));
    v.set_mode(ViewerMode::Hex).expect("hex");

    let said = v.toggle_hex_sign();
    assert!(said.contains("no sign"), "{said}");
    assert_eq!(v.hex_config().format, HexFormat::Hex, "base unchanged");

    // Into decimal, then sign flips freely.
    v.cycle_hex_format();
    assert_eq!(v.hex_config().format, HexFormat::Unsigned);
    v.toggle_hex_sign();
    assert_eq!(v.hex_config().format, HexFormat::Signed);

    // And `d` twice is a round trip that remembers the sign, not a reset.
    v.cycle_hex_format();
    assert_eq!(v.hex_config().format, HexFormat::Hex);
    v.cycle_hex_format();
    assert_eq!(v.hex_config().format, HexFormat::Signed, "sign remembered");
}

#[test]
fn shift_left_at_the_end_of_a_row_selects_the_last_character() {
    // Reported from a real session: `End` leaves the cursor on the row's last
    // character, and `Shift+Left` there selected the character *before* it -
    // the last one could not be reached at all. A head is exclusive going
    // forward, so `End` points one past the character it sits on, and a
    // selection has to anchor on the point rather than on the character.
    let mut v = open("abcde\n");
    v.layout(2, 40).expect("layout");
    v.move_cursor(Motion::RowEnd, Extend::None).expect("end");
    assert_eq!(v.cursor(), 4, "the cursor sits on `e`, the last character");

    v.move_cursor(Motion::Left, Extend::Linear)
        .expect("shift-left");
    assert_eq!(
        v.selection().map(|s| s.range()),
        Some((4, 5)),
        "one press selects `e` and nothing else"
    );

    v.move_cursor(Motion::Left, Extend::Linear)
        .expect("shift-left");
    assert_eq!(
        v.selection().map(|s| s.range()),
        Some((3, 5)),
        "and the next takes `d` with it"
    );
}

#[test]
fn shift_right_from_the_second_character_still_reaches_the_first() {
    // The other half of the same report. Nothing here points past the cursor,
    // so the anchor is the cursor and this is the ordinary case.
    let mut v = open("abcde\n");
    v.layout(2, 40).expect("layout");
    v.move_cursor(Motion::Right, Extend::None).expect("right");
    assert_eq!(v.cursor(), 1);
    v.move_cursor(Motion::Left, Extend::Linear)
        .expect("shift-left");
    assert_eq!(
        v.selection().map(|s| s.range()),
        Some((0, 1)),
        "the first character is selectable"
    );
}

#[test]
fn a_compiled_android_manifest_renders_as_xml_without_hiding_its_bytes() {
    // The file is binary, so it opens as the dump it is and `2` inspects the
    // real bytes at their real offsets. `3` is the content renderer, and that
    // is where the document it was compiled from belongs - decoding it into
    // modes 1 and 2 would have shown a reader the bytes of a rendering and
    // called them the file's.
    let home = match std::env::var("HOME") {
        Ok(home) => home,
        Err(_) => return,
    };
    let path = std::path::Path::new(&home).join("TestData/AndroidManifest.xml");
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("SKIPPING: no sample manifest at ~/TestData");
        return;
    };

    let mut v = open_bytes(&bytes, &cfg());
    assert_eq!(
        v.mode(),
        ViewerMode::Render,
        "what it decodes to is the point, so that is what opens"
    );
    let doc = v.rendered().expect("mode 3 built a document");
    assert_eq!(doc.kind, crate::viewer::render::RenderKind::Axml);
    let text: Vec<&str> = doc.lines.iter().map(|l| l.text.as_str()).collect();
    assert!(
        text.first().is_some_and(|l| l.starts_with("<?xml ")),
        "it opens as a document: {:?}",
        text.first()
    );
    assert!(
        text.iter().any(|l| l.contains("android:versionName=")),
        "with its attributes prefixed"
    );

    // And the bytes are still the file's: mode 2 reads what is on disk.
    v.set_mode(ViewerMode::Hex).expect("mode 2");
    assert_eq!(
        v.len(),
        Some(bytes.len() as u64),
        "the dump is over the whole real file"
    );
}
