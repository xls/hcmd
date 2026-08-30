//! The JSON tree: what it draws, what it folds, and what it refuses.

use super::*;

/// The rendered lines as plain text, which is what a person reads.
fn lines(text: &str) -> Vec<String> {
    render(text)
        .expect("parses")
        .into_iter()
        .map(|l| l.text)
        .collect()
}

const DOC: &str = r#"{
  "name": "hcmd",
  "version": "0.1.0",
  "count": 42,
  "beta": false,
  "missing": null,
  "targets": [
    { "kind": "bin", "name": "hcmd" },
    { "kind": "lib" }
  ],
  "empty": {}
}"#;

#[test]
fn an_object_is_a_tree_with_the_punctuation_gone() {
    assert_eq!(
        lines(DOC),
        vec![
            "{",
            "  name: \"hcmd\"",
            "  version: \"0.1.0\"",
            "  count: 42",
            "  beta: false",
            "  missing: null",
            "  targets: [",
            "    {",
            "      kind: \"bin\"",
            "      name: \"hcmd\"",
            "    }",
            "    {",
            "      kind: \"lib\"",
            "    }",
            "  ]",
            "  empty: {}",
            "}",
        ]
    );
}

#[test]
fn a_container_carries_the_summary_a_collapsed_one_shows() {
    let out = render(DOC).expect("parses");
    let summary = |at: usize| {
        out.get(at)
            .and_then(|l: &RenderLine| l.fold.as_ref())
            .map(|f| f.summary.clone())
    };
    // The root object has seven members.
    assert_eq!(summary(0).as_deref(), Some("{...} 7 keys"));
    // The array of two.
    assert_eq!(summary(6).as_deref(), Some("  targets: [...] 2 items"));
    // The first element, of two keys. The indent is part of the summary
    // because the summary replaces the whole line, indent included.
    assert_eq!(summary(7).as_deref(), Some("    {...} 2 keys"));
    // And the one-key object says "key", not "keys".
    assert_eq!(summary(11).as_deref(), Some("    {...} 1 key"));
}

#[test]
fn a_fold_covers_exactly_the_lines_between_the_brackets() {
    let out = render(DOC).expect("parses");
    let through = |at: usize| out.get(at).and_then(|l| l.fold.as_ref()).map(|f| f.through);
    assert_eq!(through(0), Some(16), "the root, up to its closing brace");
    assert_eq!(through(6), Some(14), "the array");
    assert_eq!(through(7), Some(10), "its first element");
}

#[test]
fn an_empty_container_is_not_foldable() {
    let out = render(DOC).expect("parses");
    // `empty: {}` is drawn on one line and has nothing to hide.
    let empty = out.get(15).expect("the empty object");
    assert_eq!(empty.text, "  empty: {}");
    assert!(empty.fold.is_none());
    // A document that is only an empty object is one line, for the same
    // reason.
    let bare = render("{}").expect("parses");
    assert_eq!(bare.len(), 1);
    assert_eq!(bare.first().map(|l| l.text.clone()), Some("{}".to_string()));
    assert!(bare.first().is_some_and(|l| l.fold.is_none()));
}

#[test]
fn scalars_take_the_slot_that_says_what_they_are() {
    let out = render(r#"{"s":"x","n":-1.5e3,"b":true,"z":null}"#).expect("parses");
    let slot_of = |at: usize, want: SynSlot| {
        out.get(at)
            .is_some_and(|l| l.spans.iter().any(|s| s.slot == Some(want)))
    };
    assert!(slot_of(1, SynSlot::String), "a string");
    assert!(slot_of(2, SynSlot::Number), "a number");
    assert!(slot_of(3, SynSlot::Constant), "a boolean");
    assert!(slot_of(4, SynSlot::Constant), "a null");
    // Every key is the variable slot, which is what separates it from a
    // string value at a glance.
    assert!(
        out.get(1)
            .is_some_and(|l| l.spans.iter().any(|s| s.slot == Some(SynSlot::Variable)))
    );
}

#[test]
fn a_top_level_array_and_a_top_level_scalar_both_render() {
    assert_eq!(lines("[1, 2]"), vec!["[", "  1", "  2", "]"]);
    assert_eq!(lines("  \"just a string\"  "), vec!["\"just a string\""]);
    assert_eq!(lines("42"), vec!["42"]);
}

#[test]
fn escapes_and_odd_characters_inside_strings_survive() {
    // A quote escaped inside a string does not end it, and a `\n` is shown as
    // the two characters the file holds rather than becoming a second row.
    let out = lines(r#"{"path":"C:\\tmp","said":"he said \"no\"","nl":"a\nb"}"#);
    assert_eq!(
        out,
        vec![
            "{",
            r#"  path: "C:\\tmp""#,
            r#"  said: "he said \"no\"""#,
            r#"  nl: "a\nb""#,
            "}",
        ]
    );
}

#[test]
fn a_key_with_a_colon_or_a_brace_in_it_is_still_one_key() {
    assert_eq!(
        lines(r#"{"a:b":1,"c}d":2}"#),
        vec!["{", "  a:b: 1", "  c}d: 2", "}"]
    );
}

#[test]
fn nesting_renders_at_every_depth_it_reaches() {
    let deep = "[".repeat(40) + &"]".repeat(40);
    let out = render(&deep).expect("parses");
    // 39 opening lines, the innermost `[]` on one line, and 39 closes.
    assert_eq!(out.len(), 79);
    assert_eq!(
        out.get(39).map(|l| l.text.clone()),
        Some(format!("{}[]", " ".repeat(39 * 2))),
        "the innermost pair, on one line"
    );
}

// ------------------------------------------------------------- refusals

#[test]
fn what_is_not_json_is_refused_rather_than_half_drawn() {
    assert!(render("").is_none(), "an empty file");
    assert!(render("   \n  ").is_none(), "whitespace only");
    assert!(render("not json at all").is_none());
    assert!(render("{").is_none(), "an object that never closes");
    assert!(render(r#"{"a": }"#).is_none(), "a member with no value");
    assert!(render(r#"{"a" 1}"#).is_none(), "a member with no colon");
    assert!(render("{\"a\": \"unterminated").is_none());
}

/// A file of one object per line is a real and common thing, and drawing only
/// its first object as if that were the document would be worse than text.
#[test]
fn a_stream_of_documents_is_refused_rather_than_showing_only_the_first() {
    assert!(render("{\"a\":1}\n{\"b\":2}\n").is_none());
    assert!(render("[1] [2]").is_none());
    // But trailing whitespace after one document is fine.
    assert!(render("{\"a\":1}\n\n  ").is_some());
}

#[test]
fn a_document_deeper_than_the_limit_is_refused_and_does_not_overflow_the_stack() {
    let deep = "[".repeat(MAX_DEPTH + 10) + &"]".repeat(MAX_DEPTH + 10);
    assert!(render(&deep).is_none());
    // And one far past it, which a recursive walk would not survive at all.
    let absurd = "[".repeat(200_000);
    assert!(render(&absurd).is_none());
}
