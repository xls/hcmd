//! Generating `config.toml` from the config structs.
//!
//! The design makes the structs in [`crate::config::config`] the single source
//! of truth for the file. The shape, order, section nesting and every comment
//! come from those structs' fields (through `#[derive(ConfigDoc)]`, which reads
//! the doc comments at compile time); the actual values come from serialising a
//! [`Config`] with `toml`. This module merges the two: schema order and
//! comments, with each option's value pulled from the serialised config.
//!
//! Two things a value cannot carry, and so are curated here instead:
//!
//! * The **commented example blocks** (`[[open.handlers]]`,
//!   `[viewer.highlight.lsp]`, `[terminal.sequences]`, the mode-based
//!   `[[filetypes]]`). These ship commented as examples and are not part of any
//!   default value, so there is nothing in a serialised `Config` to render them
//!   from. They live in [`curated_example`], a small per-section table, and are
//!   appended after their section.
//! * The **section headers**, which come from the field in the parent struct,
//!   not from the value.
//!
//! The schema itself is an ordered [`Vec`] of [`SchemaItem`], not a fixed
//! structure: [`config_schema`] is the built-in list, and it ends with a seam
//! where later work (a plugin registering its own `[section] key`) can append
//! more entries. The generator consumes whatever the list holds, so the
//! built-ins and any registered options are emitted from one walk.

use crate::config::Config;
use crate::panel::ColumnId;

/// One entry in the configuration schema: a section header, an array-of-tables
/// header, or a single option. Emitted in order by [`Config::describe`] (from
/// `#[derive(ConfigDoc)]`) and consumed by [`generate`].
pub enum SchemaItem {
    /// A `[path]` table header, its comment taken from the field in the parent.
    Section {
        /// The dotted TOML path, for example `panel.columns`.
        path: String,
        /// The lines that introduce the section.
        comment: String,
    },
    /// A `[[path]]` array-of-tables header (`filetypes`, `open.handlers`).
    ArraySection {
        /// The dotted TOML path.
        path: String,
        /// The lines that introduce the array.
        comment: String,
    },
    /// One `key = value` option living under `section`.
    Option {
        /// The dotted path of the section this option lives under.
        section: String,
        /// The TOML key.
        key: String,
        /// The lines that document it.
        comment: String,
    },
}

impl SchemaItem {
    /// A section header. Used by generated `describe` code.
    pub fn section(path: &str, comment: &str) -> Self {
        Self::Section {
            path: path.to_string(),
            comment: comment.to_string(),
        }
    }

    /// An array-of-tables header. Used by generated `describe` code.
    pub fn array_section(path: &str, comment: &str) -> Self {
        Self::ArraySection {
            path: path.to_string(),
            comment: comment.to_string(),
        }
    }

    /// A single option. Used by generated `describe` code.
    pub fn option(section: &str, key: &str, comment: &str) -> Self {
        Self::Option {
            section: section.to_string(),
            key: key.to_string(),
            comment: comment.to_string(),
        }
    }
}

/// Join a parent section path and a key into a dotted path. Used by generated
/// `describe` code; an empty parent (the root) yields the key alone.
pub fn join_section(parent: &str, key: &str) -> String {
    if parent.is_empty() {
        key.to_string()
    } else {
        format!("{parent}.{key}")
    }
}

/// The whole configuration schema, in file order: every table and option the
/// structs describe. Both the generated file and the unknown-key validator are
/// built from this one walk, so an option exists in exactly one place.
pub fn config_schema() -> Vec<SchemaItem> {
    let mut items = Vec::new();
    Config::describe("", &mut items);
    items
}

/// Generate the reference `config.toml` from a config value, every option
/// written live. `generate(&Config::default())` is exactly `examples/config.toml`.
pub fn generate(config: &Config) -> String {
    render(config, &|_, _| true, None)
}

/// Regenerate a user's `config.toml`: options they set are written live, the
/// rest are written commented at their default, and a version stamp is added.
///
/// `is_live(section, key)` answers "did the user's file set this?"; for an
/// array-of-tables the key is empty (the array as a whole is set or not).
pub fn generate_preserving(
    config: &Config,
    is_live: &dyn Fn(&str, &str) -> bool,
    version: &str,
) -> String {
    render(config, is_live, Some(version))
}

/// A grouped section, built from the flat schema so a section's own options are
/// all emitted before any of its sub-sections' headers, whatever order the
/// fields fall in.
struct Block {
    path: String,
    is_array: bool,
    comment: String,
    options: Vec<(String, String)>,
}

/// Walk the schema and emit the file.
fn render(config: &Config, is_live: &dyn Fn(&str, &str) -> bool, version: Option<&str>) -> String {
    let items = config_schema();
    let root =
        toml::Value::try_from(config).unwrap_or_else(|_| toml::Value::Table(toml::Table::new()));

    // Group by section path, in first-appearance order. Grouping (rather than a
    // straight walk) is what keeps `panel.name_truncate` under `[panel]` even
    // though the field falls after the `columns` sub-section in the struct: a
    // key written after a `[panel.columns]` header would otherwise land in the
    // wrong table.
    let mut blocks: Vec<Block> = Vec::new();
    let find = |path: &str, blocks: &mut Vec<Block>| -> usize {
        if let Some(i) = blocks.iter().position(|b| b.path == path) {
            i
        } else {
            blocks.push(Block {
                path: path.to_string(),
                is_array: false,
                comment: String::new(),
                options: Vec::new(),
            });
            blocks.len().saturating_sub(1)
        }
    };
    for item in &items {
        match item {
            SchemaItem::Section { path, comment } => {
                let i = find(path, &mut blocks);
                if let Some(b) = blocks.get_mut(i) {
                    b.comment = comment.clone();
                }
            }
            SchemaItem::ArraySection { path, comment } => {
                let i = find(path, &mut blocks);
                if let Some(b) = blocks.get_mut(i) {
                    b.comment = comment.clone();
                    b.is_array = true;
                }
            }
            SchemaItem::Option {
                section,
                key,
                comment,
            } => {
                let i = find(section, &mut blocks);
                if let Some(b) = blocks.get_mut(i) {
                    b.options.push((key.clone(), comment.clone()));
                }
            }
        }
    }

    // What the program would hold with nothing configured, so an open table
    // still at its defaults can be told from one the user has written in.
    let defaults = toml::Value::try_from(Config::default()).ok();
    let mut out = file_header(version);
    for block in &blocks {
        out.push('\n');
        push_comment(&mut out, &block.comment);
        if block.is_array {
            render_array(&mut out, &block.path, &root, is_live);
        } else {
            out.push('[');
            out.push_str(&block.path);
            out.push_str("]\n");
            for (key, comment) in &block.options {
                render_option(&mut out, &block.path, key, comment, &root, is_live);
            }
        }
        // An open table the user has filled in stands in for the commented
        // example of one: the example is there to show the shape, and their own
        // entries show it better.
        let filled = render_open_tables(&mut out, &block.path, &root, &defaults);
        if !filled && let Some(example) = curated_example(&block.path) {
            out.push('\n');
            out.push_str(example);
            if !example.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    out
}

/// Write out any open table belonging to `parent` that the user has put
/// something in. Answers whether it wrote anything.
///
/// These tables are left out of the schema because their keys are the user's
/// own - a terminal's escape sequence, a language's server - and no struct
/// field can name them ahead of time. Left out of the *file* as well, though,
/// they were deleted by the very run meant to bring the file up to date:
/// `[terminal.sequences]` is the way out of a terminal that eats a key, and a
/// regeneration silently took it away.
fn render_open_tables(
    out: &mut String,
    parent: &str,
    root: &toml::Value,
    defaults: &Option<toml::Value>,
) -> bool {
    let mut wrote = false;
    for path in crate::config::OPEN_TABLES {
        if join_section(parent, path.rsplit('.').next().unwrap_or(path)) != *path {
            continue;
        }
        let Some(toml::Value::Table(table)) = get_path(root, path) else {
            continue;
        };
        // Only what the user actually put there. A table still holding the
        // built-in entries is what the commented example already shows, and
        // writing it out live would pin those defaults - the same trap as
        // writing a key the user never set without its comment.
        let untouched = defaults
            .as_ref()
            .and_then(|d| get_path(d, path))
            .is_some_and(|d| d == &toml::Value::Table(table.clone()));
        if table.is_empty() || untouched {
            continue;
        }
        out.push('\n');
        out.push('[');
        out.push_str(path);
        out.push_str("]\n");
        for (key, value) in ordered_entries(table) {
            out.push_str(&format!("{} = {}\n", render_key(key), render_value(value)));
        }
        wrote = true;
    }
    wrote
}

/// A table key as TOML will read it back: bare where it can be, quoted where
/// it cannot. `ctrl+f9` is a key a person writes and not a bare TOML one.
fn render_key(key: &str) -> String {
    if !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return key.to_string();
    }
    quote_string(key)
}

/// One option line, preceded by its comment. An option whose value is absent
/// from the serialised config (a `None` with no TOML form, like
/// `panel.name_truncate` at its default) is left out, the way the reference
/// file omits it rather than writing an empty line.
fn render_option(
    out: &mut String,
    section: &str,
    key: &str,
    comment: &str,
    root: &toml::Value,
    is_live: &dyn Fn(&str, &str) -> bool,
) {
    let path = join_section(section, key);
    let Some(value) = get_path(root, &path) else {
        return;
    };
    push_comment(out, comment);
    let line = format!("{key} = {}", render_value(value));
    if is_live(section, key) {
        out.push_str(&line);
    } else {
        out.push_str("# ");
        out.push_str(&line);
    }
    out.push('\n');
}

/// The `[[path]]` blocks for an array-of-tables, from the serialised value.
/// Empty arrays render nothing; the commented example (if any) still follows.
fn render_array(
    out: &mut String,
    path: &str,
    root: &toml::Value,
    is_live: &dyn Fn(&str, &str) -> bool,
) {
    let live = is_live(path, "");
    let Some(toml::Value::Array(items)) = get_path(root, path) else {
        return;
    };
    for item in items {
        let toml::Value::Table(table) = item else {
            continue;
        };
        let mut body = format!("[[{path}]]\n");
        for (key, value) in ordered_entries(table) {
            body.push_str(&format!("{key} = {}\n", render_value(value)));
        }
        if live {
            out.push_str(&body);
        } else {
            for bl in body.lines() {
                out.push_str("# ");
                out.push_str(bl);
                out.push('\n');
            }
        }
    }
}

/// Follow a dotted path into a TOML value.
fn get_path<'a>(root: &'a toml::Value, dotted: &str) -> Option<&'a toml::Value> {
    let mut current = root;
    for part in dotted.split('.') {
        current = current.as_table()?.get(part)?;
    }
    Some(current)
}

/// Render one TOML value as it should appear on the right of an `=`.
fn render_value(value: &toml::Value) -> String {
    match value {
        toml::Value::String(s) => quote_string(s),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => fmt_float(*f),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Datetime(d) => d.to_string(),
        toml::Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(render_value).collect();
            format!("[{}]", parts.join(", "))
        }
        toml::Value::Table(table) => {
            let parts: Vec<String> = ordered_entries(table)
                .into_iter()
                .map(|(k, v)| format!("{k} = {}", render_value(v)))
                .collect();
            format!("{{ {} }}", parts.join(", "))
        }
    }
}

/// A table's entries in a readable order. A table whose keys are all column ids
/// (`width`, `min_chars`) is ordered the way the panel shows the columns rather
/// than alphabetically; every other table follows a small priority list, then
/// alphabetical, so `match` reads `ext`, `mime`, `mode` and a rule reads
/// `match` before `slot`.
fn ordered_entries(table: &toml::Table) -> Vec<(&str, &toml::Value)> {
    let mut keys: Vec<&str> = table.keys().map(String::as_str).collect();
    let all_columns = !keys.is_empty() && keys.iter().all(|k| ColumnId::from_id(k).is_some());
    if all_columns {
        keys.sort_by_key(|k| {
            ColumnId::ALL
                .iter()
                .position(|c| c.id() == *k)
                .unwrap_or(usize::MAX)
        });
    } else {
        keys.sort_by(|a, b| key_rank(a).cmp(&key_rank(b)).then_with(|| a.cmp(b)));
    }
    keys.into_iter()
        .filter_map(|k| table.get(k).map(|v| (k, v)))
        .collect()
}

/// Where a key sorts inside an inline table. Anything unlisted sorts after the
/// named ones, alphabetically among itself.
fn key_rank(key: &str) -> usize {
    match key {
        "ext" => 0,
        "mime" => 1,
        "mode" => 2,
        "match" => 3,
        "slot" => 4,
        "command" => 5,
        _ => usize::MAX,
    }
}

/// A TOML basic string, quoted and escaped.
fn quote_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A float with a decimal point, so `0.5` stays `0.5` and `1` becomes `1.0`.
fn fmt_float(f: f64) -> String {
    if f.is_finite() && f == f.trunc() {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}

/// Emit a multi-line comment as `#` lines. A blank line becomes a bare `#`, and
/// two blanks in a row collapse to one so a comment does not gap itself open.
/// The line is cleaned of the rustdoc markup that would read as noise in a
/// config file (see [`clean_comment_line`]).
fn push_comment(out: &mut String, comment: &str) {
    if comment.is_empty() {
        return;
    }
    let mut prev_blank = false;
    for raw in comment.split('\n') {
        let line = clean_comment_line(raw);
        if line.is_empty() {
            if !prev_blank {
                out.push_str("#\n");
            }
            prev_blank = true;
        } else {
            out.push_str("# ");
            out.push_str(&line);
            out.push('\n');
            prev_blank = false;
        }
    }
}

/// Strip the markup that belongs to `rustdoc` but not to a config file: the
/// `**bold**` markers, a leading blockquote `> `, and the square brackets an
/// intra-doc link wraps a `` `path` `` in. The doc comment keeps the markup for
/// `rustdoc`; only the generated file is cleaned.
fn clean_comment_line(raw: &str) -> String {
    let mut line = raw.trim_end().to_string();
    if let Some(rest) = line.strip_prefix("> ") {
        line = rest.to_string();
    }
    line = line.replace("**", "");
    // `[`Type`]` -> `` `Type` ``: drop the link brackets, keep the code span.
    line = line.replace("[`", "`").replace("`]", "`");
    line.trim_end().to_string()
}

/// The top-of-file header. `version` present stamps `written by version X`, the
/// marker the loader reads to tell a stale file from a current one; the
/// reference file (`examples/config.toml`) carries no stamp.
fn file_header(version: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str("# Holos Commander configuration.\n");
    out.push_str("# Location: ~/.config/holoscommander/config.toml\n");
    if let Some(v) = version {
        out.push_str(&format!(
            "# Generated from the config schema, written by version {v}.\n"
        ));
    }
    out.push_str("#\n");
    out.push_str("# This file is generated from the program's own configuration schema: every\n");
    out.push_str("# option, its default and the comment above it come from one place in the\n");
    out.push_str("# source, so the file cannot drift from what the program reads and cannot\n");
    out.push_str("# grow a duplicate section. `hcmd --update-config` regenerates it in place,\n");
    out.push_str("# keeping the values you set and moving the old file aside with today's date.\n");
    out.push_str("# Delete anything you do not want to override; every key is optional.\n");
    out
}

/// The commented example blocks a section ships with, keyed by section path.
///
/// These are examples, not defaults: nothing in a `Config` value would render
/// an `[[open.handlers]]` rule or a `[terminal.sequences]` line, because the
/// defaults hold none. They ship commented so they document the shape without
/// changing a setting, and the design keeps them here, curated, rather than
/// inventing default values whose only purpose is to be shown and then deleted.
fn curated_example(section: &str) -> Option<&'static str> {
    match section {
        "open.handlers" => Some(
            "# [[open.handlers]]\n\
             # match   = { ext = [\"png\", \"jpg\", \"webp\"] }\n\
             # command = [\"imv\", \"{file}\"]\n\
             #\n\
             # [[open.handlers]]\n\
             # match   = { mime = \"text/*\" }\n\
             # command = [\"$EDITOR\", \"{file}\"]\n",
        ),
        "viewer.highlight" => Some(
            "# [viewer.highlight.lsp]\n\
             # rust = \"rust-analyzer\"\n",
        ),
        "terminal" => Some(
            "# Bind a raw escape sequence to a logical key for terminals that need it.\n\
             # [terminal.sequences]\n\
             # \"shift+f5\" = \"[15;2~\"\n",
        ),
        "filetypes" => Some(
            "# Colour by mode bits rather than by name: a `.sh` without +x is data, and a\n\
             # file with no extension and +x is a program.\n\
             # [[filetypes]]\n\
             # match = { mode = \"exec\" }\n\
             # slot  = \"panel.exec_fg\"\n",
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reference_file_is_generated_not_hand_written() {
        // `examples/config.toml` IS `generate(&Config::default())`. The design
        // makes it generated so it cannot drift from the structs; this asserts
        // it. Run the suite with `HCMD_REGEN_CONFIG=1` after a config change to
        // rewrite the file, then rebuild so the embedded copy catches up.
        let generated = generate(&Config::default());
        if std::env::var_os("HCMD_REGEN_CONFIG").is_some() {
            std::fs::write("examples/config.toml", &generated).expect("rewrite the reference file");
        }
        assert_eq!(
            generated,
            crate::config::EXAMPLE_CONFIG,
            "examples/config.toml is stale; regenerate with HCMD_REGEN_CONFIG=1"
        );
    }

    #[test]
    fn a_float_keeps_its_point() {
        assert_eq!(fmt_float(0.5), "0.5");
        assert_eq!(fmt_float(1.0), "1.0");
    }

    #[test]
    fn strings_are_escaped() {
        assert_eq!(quote_string("blue"), "\"blue\"");
        assert_eq!(quote_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn column_tables_order_by_the_panel_not_the_alphabet() {
        let mut t = toml::Table::new();
        t.insert("attr".into(), toml::Value::Integer(8));
        t.insert("ext".into(), toml::Value::Integer(6));
        t.insert("size".into(), toml::Value::Integer(12));
        assert_eq!(
            render_value(&toml::Value::Table(t)),
            "{ ext = 6, size = 12, attr = 8 }"
        );
    }
}
