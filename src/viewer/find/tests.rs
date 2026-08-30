//! Quick find's tests.
//!
//! The ones that matter most are the boundary ones. A streaming searcher that
//! works on a small file and loses a match at a chunk boundary passes every
//! obvious test there is, so the boundary is tested at the real
//! [`crate::viewer::source::MAX_WINDOW`] rather than at a convenient small one.

use super::*;

use crate::viewer::decode::encoding_for;

fn enc(label: &str) -> TextEncoding {
    encoding_for(label).unwrap_or(TextEncoding::UTF8)
}

fn query(input: &str, kind: FindKind, case: QuickSearchCase) -> FindQuery {
    FindQuery {
        input: input.to_string(),
        kind,
        case,
    }
}

fn text_matcher(needle: &str) -> Matcher {
    Matcher::text(needle, TextEncoding::UTF8, false).expect("compiles")
}

// -------------------------------------------------------------- patterns ----

#[test]
fn a_hex_pattern_reads_the_spec_example_and_its_wildcards() {
    let m = Matcher::hex("DE AD BE EF").expect("compiles");
    assert_eq!(m.len(), 4);
    assert!(m.matches_at(&[0xDE, 0xAD, 0xBE, 0xEF], 0));
    assert!(!m.matches_at(&[0xDE, 0xAD, 0xBE, 0xEE], 0));

    let m = Matcher::hex("DE ?? BE EF").expect("compiles");
    assert!(m.matches_at(&[0xDE, 0x00, 0xBE, 0xEF], 0));
    assert!(m.matches_at(&[0xDE, 0xFF, 0xBE, 0xEF], 0));
    assert!(!m.matches_at(&[0xDD, 0xFF, 0xBE, 0xEF], 0));
}

#[test]
fn a_hex_pattern_accepts_the_spellings_a_person_pastes() {
    for spelling in [
        "DEADBEEF",
        "de ad be ef",
        "0xDE 0xAD 0xBE 0xEF",
        "DE,AD,BE,EF",
        "DE:AD:BE:EF",
        "$DE $AD $BE $EF",
    ] {
        let m = Matcher::hex(spelling).unwrap_or_else(|e| panic!("{spelling}: {e}"));
        assert!(
            m.matches_at(&[0xDE, 0xAD, 0xBE, 0xEF], 0),
            "{spelling} did not match"
        );
    }
}

#[test]
fn half_a_byte_is_an_error_that_says_so_rather_than_a_silent_miss() {
    let err = Matcher::hex("DEA").expect_err("odd digits");
    assert!(matches!(err, FindError::Hex(_)), "{err:?}");
    assert!(err.to_string().contains("whole bytes"), "{err}");

    let err = Matcher::hex("D?").expect_err("half a wildcard");
    assert!(err.to_string().contains("??"), "{err}");

    let err = Matcher::hex("zz").expect_err("not hex");
    assert!(matches!(err, FindError::Hex(_)), "{err:?}");
}

#[test]
fn hex_mode_reads_the_bar_as_bytes_only_when_it_really_is_bytes() {
    let hexish = query("dead", FindKind::Auto, QuickSearchCase::Insensitive);
    assert_eq!(hexish.effective_kind(true), FindKind::Hex);
    assert_eq!(
        hexish.effective_kind(false),
        FindKind::Text,
        "text mode never reads the bar as bytes"
    );

    // `zz` is not a byte pattern, so in hex mode it is still a text search
    // rather than an error - "the same bar accepts either text
    // or a hex byte pattern".
    let texty = query("zz", FindKind::Auto, QuickSearchCase::Insensitive);
    assert_eq!(texty.effective_kind(true), FindKind::Text);

    // And an explicit kind wins over the mode either way.
    let forced = query("dead", FindKind::Text, QuickSearchCase::Insensitive);
    assert_eq!(forced.effective_kind(true), FindKind::Text);
}

#[test]
fn regex_is_reachable_disabled_and_names_the_milestone_that_brings_it() {
    assert!(!FindKind::Regex.is_available());
    assert!(FindKind::Text.is_available());
    assert!(FindKind::Hex.is_available());

    let err = Matcher::compile(
        &query("a+b", FindKind::Regex, QuickSearchCase::Insensitive),
        TextEncoding::UTF8,
        false,
    )
    .expect_err("deferred");
    assert_eq!(err, FindError::RegexDeferred);
    assert!(err.to_string().contains("v0.7"), "{err}");
    assert!(err.to_string().contains("regex search"), "{err}");

    // And the toggle really does reach it, so the note is visible.
    let mut ring = FindKind::Auto;
    let mut seen = Vec::new();
    for _ in 0..FindKind::ALL.len() {
        seen.push(ring);
        ring = ring.next();
    }
    assert!(seen.contains(&FindKind::Regex));
    assert_eq!(ring, FindKind::Auto, "the toggle is a ring");
}

#[test]
fn a_text_pattern_is_encoded_into_the_files_encoding_and_not_into_utf8() {
    // cp437's `é` is one byte, 0x82 - nothing like UTF-8's C3 A9.
    let m = Matcher::text("é", enc("cp437"), true).expect("compiles");
    assert_eq!(m.len(), 1);
    assert!(m.matches_at(&[0x82], 0));

    // windows-1252's is 0xE9.
    let m = Matcher::text("é", enc("windows-1252"), true).expect("compiles");
    assert!(m.matches_at(&[0xE9], 0));

    // UTF-16LE is laid out by hand, because encoding_rs's `encode` would hand
    // back UTF-8 for it.
    let m = Matcher::text("hi", enc("utf-16le"), true).expect("compiles");
    assert_eq!(m.len(), 4);
    assert!(m.matches_at(b"h\0i\0", 0));
    let m = Matcher::text("hi", enc("utf-16be"), true).expect("compiles");
    assert!(m.matches_at(b"\0h\0i", 0));
}

#[test]
fn a_character_the_encoding_cannot_write_is_refused_with_a_reason() {
    let err = Matcher::text("日", enc("cp437"), true).expect_err("no kanji in cp437");
    assert!(matches!(err, FindError::Unencodable { .. }), "{err:?}");
    assert!(err.to_string().contains("cp437"), "{err}");
}

#[test]
fn case_insensitive_matching_folds_ascii_and_latin_alike() {
    let m = Matcher::text("café", TextEncoding::UTF8, false).expect("compiles");
    assert!(m.find_in("a CAFÉ here".as_bytes(), 0).is_some());
    assert!(m.find_in("a Café here".as_bytes(), 0).is_some());
    assert!(m.find_in("a cafe here".as_bytes(), 0).is_none());

    let m = Matcher::text("café", TextEncoding::UTF8, true).expect("compiles");
    assert!(m.find_in("a CAFÉ here".as_bytes(), 0).is_none());
    assert!(m.find_in("a café here".as_bytes(), 0).is_some());
}

#[test]
fn case_folding_works_in_a_single_byte_encoding_too() {
    let m = Matcher::text("é", enc("windows-1252"), false).expect("compiles");
    assert!(m.matches_at(&[0xE9], 0), "lower case");
    assert!(m.matches_at(&[0xC9], 0), "and upper case, which is 0xC9");
}

#[test]
fn a_case_change_that_is_not_one_character_for_one_matches_literally() {
    // `ß` upper-cases to `SS`, which no per-byte class can express. Matching it
    // literally is right; inventing a two-byte alternative would be wrong.
    let m = Matcher::text("ß", TextEncoding::UTF8, false).expect("compiles");
    assert!(m.find_in("straße".as_bytes(), 0).is_some());
    assert!(m.find_in("STRASSE".as_bytes(), 0).is_none());
}

#[test]
fn smart_case_is_the_same_rule_the_panel_uses() {
    let lower = query("tho", FindKind::Text, QuickSearchCase::Smart);
    assert!(!lower.case_sensitive());
    let upper = query("Tho", FindKind::Text, QuickSearchCase::Smart);
    assert!(upper.case_sensitive(), "the ripgrep convention");
    let default = query("Tho", FindKind::Text, QuickSearchCase::Insensitive);
    assert!(!default.case_sensitive(), "case-insensitive by default");
}

#[test]
fn a_pattern_longer_than_a_window_could_carry_is_refused() {
    let long = "x".repeat(MAX_PATTERN + 1);
    let err = Matcher::text(&long, TextEncoding::UTF8, true).expect_err("too long");
    assert!(matches!(err, FindError::TooLong { .. }), "{err:?}");
    const {
        // The overlap rule needs the pattern to fit in a window with room to
        // spare, or a streaming search could not make progress at all.
        assert!(MAX_PATTERN < MAX_WINDOW);
    }
}

#[test]
fn an_empty_bar_compiles_to_the_prompt_and_not_to_a_match_on_everything() {
    let err = Matcher::compile(
        &query("", FindKind::Auto, QuickSearchCase::Insensitive),
        TextEncoding::UTF8,
        false,
    )
    .expect_err("empty");
    assert_eq!(err, FindError::Empty);
}

// -------------------------------------------------------------- scanning ----

#[test]
fn find_in_and_rfind_in_agree_about_where_the_matches_are() {
    let m = text_matcher("ab");
    let hay = b"xxabxxabxx";
    assert_eq!(m.find_in(hay, 0), Some(2));
    assert_eq!(m.find_in(hay, 3), Some(6));
    assert_eq!(m.find_in(hay, 7), None);
    assert_eq!(m.rfind_in(hay, hay.len()), Some(6));
    assert_eq!(m.rfind_in(hay, 6), Some(2));
    assert_eq!(m.rfind_in(hay, 2), None);
    assert_eq!(m.matches_in(hay), vec![2..4, 6..8]);
}

#[test]
fn a_match_at_the_very_end_of_the_haystack_is_found() {
    let m = text_matcher("end");
    assert_eq!(m.find_in(b"the end", 0), Some(4));
    assert_eq!(m.rfind_in(b"the end", 7), Some(4));
    assert_eq!(m.find_in(b"en", 0), None, "a truncated match is not one");
}

/// A file with one needle deliberately laid across the window boundary.
fn straddling_file(needle: &str, at: u64) -> Vec<u8> {
    let len = (at as usize)
        .saturating_add(needle.len())
        .saturating_add(4096);
    let mut data = vec![b'.'; len];
    let start = at as usize;
    for (i, b) in needle.as_bytes().iter().enumerate() {
        if let Some(slot) = data.get_mut(start.saturating_add(i)) {
            *slot = *b;
        }
    }
    data
}

#[test]
fn a_match_straddling_the_window_boundary_is_found_going_forward() {
    // The bug every streaming searcher has: this needle is in neither window.
    let boundary = MAX_WINDOW as u64;
    for offset in [boundary - 3, boundary - 1, boundary, boundary - 6] {
        let data = straddling_file("NEEDLE", offset);
        let mut source = Source::from_memory(data).expect("open");
        let m = text_matcher("NEEDLE");
        assert_eq!(
            find_forward(&mut source, &m, 0, FIND_READ_BUDGET).expect("search"),
            Found::Hit(offset),
            "a needle at {offset} across the {boundary} boundary was lost"
        );
    }
}

#[test]
fn a_match_straddling_the_window_boundary_is_found_going_backward() {
    let boundary = MAX_WINDOW as u64;
    for offset in [boundary - 3, boundary - 1, boundary] {
        let data = straddling_file("NEEDLE", offset);
        let end = data.len() as u64;
        let mut source = Source::from_memory(data).expect("open");
        let m = text_matcher("NEEDLE");
        assert_eq!(
            find_backward(&mut source, &m, end, FIND_READ_BUDGET).expect("search"),
            Found::Hit(offset),
            "a needle at {offset} across the {boundary} boundary was lost backwards"
        );
    }
}

#[test]
fn a_straddling_match_is_counted_exactly_once_by_the_background_scan() {
    let boundary = MAX_WINDOW as u64;
    let data = straddling_file("NEEDLE", boundary - 3);
    let source = Source::from_memory(data).expect("open");
    let m = text_matcher("NEEDLE");
    let (hits, total, done) = run_scan(source, m, MAX_WINDOW as u64);
    assert_eq!(hits, vec![boundary - 3]);
    assert_eq!(total, 1, "found once, not twice, despite the overlap");
    assert!(done);
}

#[test]
fn every_match_in_a_file_is_counted_however_the_windows_fall() {
    // Needles at irregular spacing, some of them across boundaries.
    let mut data = vec![b'.'; 700_000];
    // 262_141 straddles the 262_144 window boundary; its neighbours sit either
    // side of it, so a scan that mishandles the overlap loses or repeats one.
    let places = [0_u64, 12_345, 262_141, 262_147, 262_153, 524_286, 699_994];
    for at in places {
        for (i, b) in b"NEEDLE".iter().enumerate() {
            if let Some(slot) = data.get_mut((at as usize).saturating_add(i)) {
                *slot = *b;
            }
        }
    }
    let source = Source::from_memory(data).expect("open");
    let m = text_matcher("NEEDLE");
    let (hits, total, done) = run_scan(source, m, MAX_WINDOW as u64);
    assert!(done);
    assert_eq!(total, hits.len() as u64);
    assert_eq!(
        hits,
        vec![0, 12_345, 262_141, 262_147, 262_153, 524_286, 699_994]
    );
    assert!(
        hits.windows(2).all(|w| match w {
            [a, b] => a < b,
            _ => true,
        }),
        "hits arrive ascending"
    );
}

#[test]
fn a_small_chunk_still_finds_the_straddlers() {
    // The scan's chunk is configurable; a tiny one puts a boundary every few
    // bytes, which is the same bug at a different scale.
    let mut data = vec![b'.'; 40_000];
    for at in (0..40_000_usize).step_by(997) {
        for (i, b) in b"NEEDLE".iter().enumerate() {
            if let Some(slot) = data.get_mut(at.saturating_add(i)) {
                *slot = *b;
            }
        }
    }
    let expected = (0..40_000_u64)
        .step_by(997)
        .filter(|at| *at as usize + 6 <= 40_000);
    let source = Source::from_memory(data).expect("open");
    let m = text_matcher("NEEDLE");
    let (hits, total, done) = run_scan(source, m, 4_096);
    assert!(done);
    assert_eq!(hits, expected.collect::<Vec<_>>());
    assert_eq!(total, hits.len() as u64);
}

#[test]
fn a_hex_wildcard_matches_across_a_window_boundary_too() {
    let boundary = MAX_WINDOW as u64;
    let mut data = vec![b'.'; 300_000];
    let at = (boundary - 2) as usize;
    for (i, b) in [0xDE_u8, 0x01, 0xBE, 0xEF].iter().enumerate() {
        if let Some(slot) = data.get_mut(at.saturating_add(i)) {
            *slot = *b;
        }
    }
    let mut source = Source::from_memory(data).expect("open");
    let m = Matcher::hex("DE ?? BE EF").expect("compiles");
    assert_eq!(
        find_forward(&mut source, &m, 0, FIND_READ_BUDGET).expect("search"),
        Found::Hit(boundary - 2)
    );
}

#[test]
fn stepping_forward_from_a_hit_finds_the_next_one_and_then_stops() {
    let mut source = Source::from_memory(b"ab..ab..ab".to_vec()).expect("open");
    let m = text_matcher("ab");
    assert_eq!(
        find_forward(&mut source, &m, 0, FIND_READ_BUDGET).expect("search"),
        Found::Hit(0)
    );
    assert_eq!(
        find_forward(&mut source, &m, 1, FIND_READ_BUDGET).expect("search"),
        Found::Hit(4)
    );
    assert_eq!(
        find_forward(&mut source, &m, 9, FIND_READ_BUDGET).expect("search"),
        Found::None
    );
    assert_eq!(
        find_backward(&mut source, &m, 8, FIND_READ_BUDGET).expect("search"),
        Found::Hit(4)
    );
    assert_eq!(
        find_backward(&mut source, &m, 0, FIND_READ_BUDGET).expect("search"),
        Found::None
    );
}

#[test]
fn a_search_that_runs_out_of_budget_says_where_to_resume() {
    // A budget of one window over a file of three cannot reach the end.
    let mut data = vec![b'.'; MAX_WINDOW * 3];
    for (i, b) in b"NEEDLE".iter().enumerate() {
        if let Some(slot) = data.get_mut(MAX_WINDOW * 2 + i) {
            *slot = *b;
        }
    }
    let mut source = Source::from_memory(data).expect("open");
    let m = text_matcher("NEEDLE");
    let first = find_forward(&mut source, &m, 0, 1).expect("search");
    let Found::Budget(resume) = first else {
        panic!("expected a budget stop, got {first:?}");
    };
    assert!(resume > 0 && resume <= MAX_WINDOW as u64);
    // Resuming from where it said finds the needle, so nothing was skipped.
    assert_eq!(
        find_forward(&mut source, &m, resume, FIND_READ_BUDGET).expect("search"),
        Found::Hit(MAX_WINDOW as u64 * 2)
    );
}

#[test]
fn searching_a_file_with_no_match_at_all_ends_rather_than_looping() {
    let mut source = Source::from_memory(vec![b'.'; 100_000]).expect("open");
    let m = text_matcher("NEEDLE");
    assert_eq!(
        find_forward(&mut source, &m, 0, FIND_READ_BUDGET).expect("search"),
        Found::None
    );
    assert_eq!(
        find_backward(&mut source, &m, 100_000, FIND_READ_BUDGET).expect("search"),
        Found::None
    );
    assert_eq!(
        find_forward(&mut source, &m, 999_999, FIND_READ_BUDGET).expect("search"),
        Found::None,
        "past the end is not an error"
    );
}

#[test]
fn a_utf16_file_is_searched_with_utf16_bytes() {
    let text = "hello wörld\n";
    let bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut source = Source::from_memory(bytes).expect("open");
    let m = Matcher::text("wörld", enc("utf-16le"), false).expect("compiles");
    assert_eq!(
        find_forward(&mut source, &m, 0, FIND_READ_BUDGET).expect("search"),
        Found::Hit(12),
        "six UTF-16 code units in"
    );
}

// -------------------------------------------------------- the counter task --

fn run_scan(source: Source, matcher: Matcher, chunk: u64) -> (Vec<u64>, u64, bool) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    rt.block_on(async move {
        let (tx, mut rx) = mpsc::channel(FIND_CHANNEL_DEPTH);
        let cancel = Arc::new(AtomicBool::new(false));
        let handle = tokio::task::spawn_blocking(move || {
            scan(ViewerId(1), 7, source, matcher, chunk, cancel, tx);
        });
        let mut find = Find::default();
        // The bar has to be at generation 7 for the batches to be accepted,
        // which is the point of the generation.
        for _ in 0..7 {
            find.push('x');
        }
        let mut done = false;
        while let Some(batch) = rx.recv().await {
            assert!(find.apply(&batch), "generation {}", batch.generation);
            assert_eq!(batch.error, None);
            done |= batch.done;
        }
        handle.await.expect("task");
        (find.hits().to_vec(), find.total(), done)
    })
}

#[tokio::test]
async fn a_stale_batch_from_the_previous_keystroke_is_dropped() {
    let mut find = Find::default();
    find.push('a');
    let stale = FindBatch {
        id: ViewerId(1),
        generation: find.generation().saturating_sub(1),
        scanned: 10,
        hits: vec![1, 2, 3],
        total: 3,
        done: true,
        error: None,
    };
    assert!(!find.apply(&stale), "a batch from the previous query");
    assert_eq!(find.total(), 0);
    assert!(find.hits().is_empty());

    let fresh = FindBatch {
        generation: find.generation(),
        ..stale
    };
    assert!(find.apply(&fresh));
    assert_eq!(find.total(), 3);
    assert_eq!(find.hits(), [1, 2, 3]);
    assert!(find.is_complete());
}

#[tokio::test]
async fn cancelling_before_the_counter_starts_sends_nothing() {
    let source = Source::from_memory(vec![b'a'; MAX_WINDOW * 8]).expect("open");
    let (tx, mut rx) = mpsc::channel(FIND_CHANNEL_DEPTH);
    let cancel = Arc::new(AtomicBool::new(true));
    tokio::task::spawn_blocking(move || {
        scan(
            ViewerId(1),
            0,
            source,
            text_matcher("a"),
            MAX_WINDOW as u64,
            cancel,
            tx,
        );
    })
    .await
    .expect("task");
    assert!(rx.recv().await.is_none(), "nothing was ever sent");
}

/// The half that matters on a large file: the flag goes up while the counter
/// is already running, and the window loop has to see it.
///
/// A counter that reads the flag once on the way in and never again passes the
/// test above and keeps a whole file's worth of reading going after the find
/// bar has closed, so this one cancels only once a batch has arrived and
/// asserts it stopped a long way short of the end.
///
/// The bound is not a race: the channel holds `FIND_CHANNEL_DEPTH` batches and
/// the counter blocks once it is full, so at most that many windows plus the
/// one in flight can have been read before the flag was noticed.
#[tokio::test]
async fn cancelling_mid_count_stops_it_well_before_the_end() {
    // No byte matches, so the work is the reading rather than the hit list.
    const WINDOW: u64 = 4096;
    const WINDOWS: u64 = 512;
    let len = WINDOW.saturating_mul(WINDOWS);
    let source = Source::from_memory(vec![b'b'; len as usize]).expect("open");
    let (tx, mut rx) = mpsc::channel(FIND_CHANNEL_DEPTH);
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    let handle = tokio::task::spawn_blocking(move || {
        scan(
            ViewerId(1),
            0,
            source,
            text_matcher("a"),
            WINDOW,
            cancel,
            tx,
        );
    });

    let first = rx.recv().await.expect("the first window was reported");
    flag.store(true, Ordering::Relaxed);
    let mut batches = 1_usize;
    let mut scanned = first.scanned;
    let mut done = first.done;
    while let Some(batch) = rx.recv().await {
        batches = batches.saturating_add(1);
        scanned = scanned.max(batch.scanned);
        done |= batch.done;
    }
    handle.await.expect("task");

    let ceiling = FIND_CHANNEL_DEPTH.saturating_add(2);
    assert!(
        batches <= ceiling,
        "{batches} batches for a {WINDOWS}-window file: the counter did not \
         stop when the flag went up"
    );
    assert!(scanned < len, "it read the whole {len}-byte file anyway");
    assert!(!done, "a cancelled count never reports itself complete");
}

#[tokio::test]
async fn the_hit_list_is_capped_however_many_matches_the_file_holds() {
    // Every byte is a match, so the file holds far more than the cap.
    let source = Source::from_memory(vec![b'a'; MAX_HITS * 3]).expect("open");
    let (tx, mut rx) = mpsc::channel(FIND_CHANNEL_DEPTH);
    let cancel = Arc::new(AtomicBool::new(false));
    let handle = tokio::task::spawn_blocking(move || {
        scan(
            ViewerId(1),
            0,
            source,
            text_matcher("a"),
            MAX_WINDOW as u64,
            cancel,
            tx,
        );
    });
    let mut find = Find::default();
    while let Some(batch) = rx.recv().await {
        assert!(batch.hits.len() <= MAX_HITS);
        find.apply(&batch);
    }
    handle.await.expect("task");
    assert_eq!(find.total(), (MAX_HITS * 3) as u64, "the count is complete");
    assert_eq!(
        find.hits().len(),
        MAX_HITS,
        "and the list of offsets is not"
    );
}

// --------------------------------------------------------- the find bar -----

#[test]
fn typing_invalidates_the_previous_search_rather_than_mixing_the_counts() {
    let mut find = Find::default();
    find.show();
    assert!(find.is_open());
    assert!(find.push('a'));
    let first = find.generation();
    find.apply(&FindBatch {
        id: ViewerId(1),
        generation: first,
        scanned: 100,
        hits: vec![5],
        total: 1,
        done: true,
        error: None,
    });
    assert_eq!(find.total(), 1);

    assert!(find.push('b'));
    assert!(find.generation() > first, "a new search");
    assert_eq!(find.total(), 0, "and none of the old count survives");
    assert!(find.hits().is_empty());
    assert!(!find.is_complete());
    assert_eq!(find.scanned(), 0);
}

#[test]
fn backspace_on_an_empty_bar_changes_nothing() {
    let mut find = Find::default();
    assert!(!find.backspace());
    assert_eq!(find.generation(), 0);
    assert!(find.push('x'));
    assert!(find.backspace());
    assert_eq!(find.input(), "");
}

#[test]
fn esc_closes_the_bar_and_keeps_the_position() {
    let mut find = Find::default();
    find.show();
    find.set_input("needle");
    find.compile(TextEncoding::UTF8, false);
    find.set_current(Some(1_234));
    find.hide();
    assert!(!find.is_open());
    assert_eq!(
        find.current(),
        Some(1_234),
        "Esc closes the bar and keeps position"
    );
    assert!(
        find.matcher().is_some(),
        "and `n` still knows what it is looking for"
    );
}

#[test]
fn the_counter_says_when_it_is_still_filling_in() {
    let mut find = Find::default();
    find.set_input("a");
    find.compile(TextEncoding::UTF8, false);
    find.apply(&FindBatch {
        id: ViewerId(1),
        generation: find.generation(),
        scanned: 50,
        hits: vec![10, 20, 30],
        total: 3,
        done: false,
        error: None,
    });
    find.set_current(Some(20));
    assert_eq!(find.counter_text().as_deref(), Some("2/3+"));
    assert_eq!(find.match_number(), Some(2));

    find.apply(&FindBatch {
        id: ViewerId(1),
        generation: find.generation(),
        scanned: 100,
        hits: vec![40],
        total: 4,
        done: true,
        error: None,
    });
    assert_eq!(
        find.counter_text().as_deref(),
        Some("2/4"),
        "the + goes when the scan finishes"
    );
}

#[test]
fn a_search_with_nothing_to_find_says_so_once_it_is_sure() {
    let mut find = Find::default();
    find.set_input("zzz");
    find.compile(TextEncoding::UTF8, false);
    assert_eq!(find.counter_text().as_deref(), Some("0+"));
    find.apply(&FindBatch {
        id: ViewerId(1),
        generation: find.generation(),
        scanned: 100,
        hits: Vec::new(),
        total: 0,
        done: true,
        error: None,
    });
    assert_eq!(find.counter_text().as_deref(), Some("no matches"));
}

#[test]
fn the_bar_shows_the_buffer_its_case_mode_and_the_count() {
    let mut find = Find::default();
    find.set_input("café");
    find.compile(TextEncoding::UTF8, false);
    assert_eq!(find.bar_text(), "find: café [aa]  0+");

    find.cycle_case();
    assert_eq!(find.case(), QuickSearchCase::Sensitive);
    find.compile(TextEncoding::UTF8, false);
    assert_eq!(find.bar_text(), "find: café [Aa]  0+");
}

#[test]
fn the_bar_shows_why_a_pattern_does_not_compile() {
    let mut find = Find::default();
    find.set_kind(FindKind::Hex);
    find.set_input("DEA");
    find.compile(TextEncoding::UTF8, true);
    let bar = find.bar_text();
    assert!(bar.contains("[hex]"), "{bar}");
    assert!(bar.contains("whole bytes"), "{bar}");
    assert!(find.matcher().is_none());

    find.set_kind(FindKind::Regex);
    find.set_input("a+");
    find.compile(TextEncoding::UTF8, false);
    let bar = find.bar_text();
    assert!(bar.contains("[regex]"), "{bar}");
    assert!(bar.contains("v0.7"), "{bar}");
}

#[test]
fn an_untyped_bar_shows_no_error_because_a_prompt_is_not_a_failure() {
    let find = Find::default();
    assert_eq!(find.error(), Some(&FindError::Empty));
    assert_eq!(find.bar_text(), "find:  [aa]");
    assert_eq!(find.counter_text(), None);
}

// ------------------------------------------------------------ highlights ----

#[test]
fn a_lines_matches_come_back_as_ranges_into_the_decoded_text() {
    let m = text_matcher("ab");
    let ranges = match_ranges_in_line(&m, TextEncoding::UTF8, b"xxabxxab");
    assert_eq!(ranges, vec![2..4, 6..8]);
}

#[test]
fn a_range_is_in_decoded_bytes_and_not_in_file_bytes() {
    // In cp437 each byte is one character, but `é` decodes to two UTF-8 bytes,
    // so the match's decoded offset is not its byte offset.
    let cp437 = enc("cp437");
    let m = Matcher::text("ab", cp437, true).expect("compiles");
    // 0x82 is `é`; the raw line is `ééab`.
    let ranges = match_ranges_in_line(&m, cp437, &[0x82, 0x82, b'a', b'b']);
    assert_eq!(ranges, vec![4..6], "four decoded bytes of `éé` come first");
}

#[test]
fn a_hex_match_that_lands_inside_a_character_highlights_the_whole_character() {
    // The second byte of `é` (C3 A9) alone: half a character, and a highlight
    // cannot cover half a character.
    let m = Matcher::hex("A9").expect("compiles");
    let ranges = match_ranges_in_line(&m, TextEncoding::UTF8, "aéb".as_bytes());
    assert_eq!(ranges.len(), 1);
    let range = ranges.first().expect("one range");
    let decoded = "aéb";
    assert_eq!(decoded.get(range.clone()), Some("é"));
}

#[test]
fn highlighting_a_line_needs_no_io_and_is_bounded() {
    let m = text_matcher("a");
    let line = vec![b'a'; MAX_LINE_MATCHES * 2];
    assert_eq!(m.matches_in(&line).len(), MAX_LINE_MATCHES);
    assert_eq!(
        match_ranges_in_line(&m, TextEncoding::UTF8, &line).len(),
        MAX_LINE_MATCHES
    );
}

// ------------------------------------------------ the bar, driving a file ---

#[test]
fn typing_moves_the_current_match_forward_one_character_at_a_time() {
    let mut source = Source::from_memory(b"..needle..nettle..".to_vec()).expect("open");
    let mut find = Find::default();
    find.show();

    // typing searches immediately.
    find.push('n');
    let got = find
        .search(&mut source, TextEncoding::UTF8, false, 0, FIND_READ_BUDGET)
        .expect("search");
    assert_eq!(got, Found::Hit(2));
    assert_eq!(find.current(), Some(2));

    // `ne` is still the same first match...
    find.push('e');
    find.search(&mut source, TextEncoding::UTF8, false, 0, FIND_READ_BUDGET)
        .expect("search");
    assert_eq!(find.current(), Some(2));

    // ...and `net` jumps to the second word as the character lands.
    find.push('t');
    find.search(&mut source, TextEncoding::UTF8, false, 0, FIND_READ_BUDGET)
        .expect("search");
    assert_eq!(find.current(), Some(10));

    // A pattern that matches nothing leaves no current match, and does not
    // close the bar or throw anything away.
    find.push('z');
    let got = find
        .search(&mut source, TextEncoding::UTF8, false, 0, FIND_READ_BUDGET)
        .expect("search");
    assert_eq!(got, Found::None);
    assert_eq!(find.current(), None);
    assert!(find.is_open());
    assert_eq!(find.input(), "netz");
}

#[test]
fn n_and_shift_n_walk_the_matches_and_stop_at_the_ends() {
    let mut source = Source::from_memory(b"ab..ab..ab".to_vec()).expect("open");
    let mut find = Find::default();
    find.set_input("ab");
    find.search(&mut source, TextEncoding::UTF8, false, 0, FIND_READ_BUDGET)
        .expect("search");
    assert_eq!(find.current(), Some(0));

    assert_eq!(
        find.next_match(&mut source, 0, FIND_READ_BUDGET)
            .expect("n"),
        Found::Hit(4)
    );
    assert_eq!(
        find.next_match(&mut source, 0, FIND_READ_BUDGET)
            .expect("n"),
        Found::Hit(8)
    );
    assert_eq!(
        find.next_match(&mut source, 0, FIND_READ_BUDGET)
            .expect("n"),
        Found::None,
        "the end does not wrap silently"
    );
    assert_eq!(find.current(), Some(8), "and the position is kept");

    assert_eq!(
        find.prev_match(&mut source, 0, FIND_READ_BUDGET)
            .expect("shift+n"),
        Found::Hit(4)
    );
    assert_eq!(
        find.prev_match(&mut source, 0, FIND_READ_BUDGET)
            .expect("shift+n"),
        Found::Hit(0)
    );
    assert_eq!(
        find.prev_match(&mut source, 0, FIND_READ_BUDGET)
            .expect("shift+n"),
        Found::None
    );
}

#[test]
fn stepping_with_no_pattern_does_nothing_rather_than_failing() {
    let mut source = Source::from_memory(b"abc".to_vec()).expect("open");
    let mut find = Find::default();
    assert_eq!(
        find.next_match(&mut source, 0, FIND_READ_BUDGET)
            .expect("n"),
        Found::None
    );
    assert!(
        find.job(ViewerId(1), source, 4_096, Arc::new(AtomicBool::new(false)))
            .is_none(),
        "and there is no counter to spawn for a pattern that does not compile"
    );
}

#[test]
fn the_counter_job_carries_the_generation_the_bar_is_on() {
    let source = Source::from_memory(b"aaa".to_vec()).expect("open");
    let mut find = Find::default();
    find.set_input("a");
    find.compile(TextEncoding::UTF8, false);
    let job = find
        .job(ViewerId(3), source, 4_096, Arc::new(AtomicBool::new(false)))
        .expect("a job");
    assert_eq!(job.generation, find.generation());
    assert_eq!(job.id, ViewerId(3));
}

#[test]
fn the_bar_says_when_it_is_reading_the_input_as_bytes() {
    let mut find = Find::default();
    find.set_input("dead");
    // Text mode: plain text, and the bar does not shout about it.
    find.compile(TextEncoding::UTF8, false);
    assert_eq!(find.bar_text(), "find: dead [aa]  0+");
    // Hex mode: the same bar, read as four bytes, and it says so.
    find.compile(TextEncoding::UTF8, true);
    assert!(find.bar_text().contains("[hex]"), "{}", find.bar_text());
    assert_eq!(find.matcher().map(Matcher::len), Some(2));
}
