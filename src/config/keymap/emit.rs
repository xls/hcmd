//! Regenerating `keymap.toml`, keeping the bindings the user set.
//!
//! The parallel to [`crate::config::emit`], with one deliberate difference the
//! design insists on: `config.toml` is generated from the config structs, so
//! its single source of truth is Rust. `keymap.toml` already has a single
//! source of truth - the shipped `examples/keymap.toml`, embedded as
//! [`crate::config::EXAMPLE_KEYMAP`] and used as the compiled-in layout - so
//! there is nothing to move into Rust and nothing to derive. This module keeps
//! that file as the canonical layout and rewrites only two things over it: the
//! binding list of any action the user set, and whether each action reads live
//! or commented.
//!
//! The model matches `config.toml` exactly. An **uncommented** action line is
//! an override: the keymap loader replaces the built-in bindings of every
//! action a user file names, so a live line pins that action. A **commented**
//! line is inert and keeps tracking the built-in, including bindings added in
//! later versions. So a regeneration writes the actions the user set live, at
//! the bindings the user gave them, and every other action commented at its
//! default - which is what lets a new action reach an old file and what makes
//! "the only thing you lose is what you commented out" true.

use std::collections::HashMap;

/// The actions a user's `keymap.toml` states live, keyed by `(section, action)`
/// and carrying the exact binding strings the file gave them.
///
/// Read from the parsed TOML, so a commented line is simply not here - that is
/// the one way a regeneration drops something the user wrote, and it is the same
/// contract `config.toml` keeps. An unparsable file yields an empty map, which
/// renders the fresh canonical layout, exactly the fallback the loader uses.
fn live_bindings(user_text: &str) -> HashMap<(String, String), Vec<String>> {
    let mut map = HashMap::new();
    let Ok(doc) = toml::from_str::<toml::Table>(user_text) else {
        return map;
    };
    for (section, value) in &doc {
        let Some(table) = value.as_table() else {
            continue;
        };
        for (action, bindings) in table {
            let Some(list) = bindings.as_array() else {
                continue;
            };
            let strings: Vec<String> = list
                .iter()
                .filter_map(|b| b.as_str().map(str::to_string))
                .collect();
            map.insert((section.clone(), action.clone()), strings);
        }
    }
    map
}

/// The canonical reference `keymap.toml`, every action live. This is exactly
/// [`crate::config::EXAMPLE_KEYMAP`]; a test asserts the two stay byte-identical
/// so the generator cannot drift from the file it regenerates against.
pub fn generate() -> String {
    render(&live_bindings(crate::config::EXAMPLE_KEYMAP), None).text
}

/// Regenerate a user's `keymap.toml`: the actions they set live are written
/// live at their own bindings, every other action is written commented at its
/// default, and a version stamp is added.
///
/// `user_text` is parsed tolerantly; a file that will not parse is treated as
/// having set nothing, so the fresh canonical layout is written and the old
/// file is what the caller has already moved aside.
pub fn generate_preserving(user_text: &str, version: &str) -> Regenerated {
    render(&live_bindings(user_text), Some(version))
}

/// A regenerated `keymap.toml`, and what could not be placed in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Regenerated {
    /// The file to write.
    pub text: String,
    /// `section.action` for every binding the user wrote that the layout never
    /// asked about, so the caller can say so instead of dropping it in silence.
    pub unplaced: Vec<String>,
}

/// Walk the canonical layout and emit the file, live where the user spoke and
/// commented everywhere else, injecting the version stamp into the header.
fn render(live: &HashMap<(String, String), Vec<String>>, version: Option<&str>) -> Regenerated {
    let template = crate::config::EXAMPLE_KEYMAP;
    let mut lines: Vec<String> = Vec::new();
    let mut section = String::new();
    let mut injected = false;
    let mut used: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();

    for line in template.lines() {
        let trimmed = line.trim_start();

        // A `[section]` header: track it for the live lookup and keep it as
        // written. Section headers are always live - they carry no binding to
        // override, and a commented header would strand the lines below it.
        if trimmed.starts_with('[') {
            if let Some(name) = trimmed.strip_prefix('[').and_then(|r| r.split(']').next()) {
                section = name.trim().to_string();
            }
            lines.push(line.to_string());
            continue;
        }

        // A comment or a blank line: verbatim. The header stamp is injected
        // after the location line, where the design wants it read first.
        if trimmed.is_empty() || trimmed.starts_with('#') {
            lines.push(line.to_string());
            if let Some(v) = version
                && !injected
                && trimmed.starts_with("# Location:")
            {
                lines.push("#".to_string());
                lines.push(format!(
                    "# Regenerated by `hcmd --update-config`, written by version {v}. The bindings"
                ));
                lines.push(
                    "# you set stay live; every action you have not touched is left commented so"
                        .to_string(),
                );
                lines.push(
                    "# it keeps tracking the built-in default, including actions added later."
                        .to_string(),
                );
                injected = true;
            }
            continue;
        }

        // A binding line, `action = [ ... ]`. Everything else in a section is
        // one of these; nothing uncommented is neither a header nor a binding.
        let Some(eq) = line.find('=') else {
            lines.push(line.to_string());
            continue;
        };
        let action = line[..eq].trim().to_string();
        // The user's section first, then the action wherever they put it. The
        // loader takes an action in any context it is valid for - the file
        // itself shows `[global] hotlist` - so a binding written somewhere
        // other than where this layout happens to declare it is a binding, not
        // a mistake, and keying only on the pair silently threw it away.
        let key = if live.contains_key(&(section.clone(), action.clone())) {
            Some((section.clone(), action.clone()))
        } else {
            live.keys().find(|(_, other)| *other == action).cloned()
        };
        if let Some(key) = &key {
            used.insert(key.clone());
        }
        match key.and_then(|key| live.get(&key)) {
            Some(user_list) => {
                // The user set this action. Keep the line verbatim - alignment
                // and inline comment intact - when their bindings are the
                // default anyway, so the canonical layout reproduces itself
                // byte-for-byte; otherwise substitute their list, preserving the
                // alignment up to the `=` and dropping the comment that would
                // now describe a default they have replaced.
                if *user_list == extract_list(&line[eq + 1..]) {
                    lines.push(line.to_string());
                } else {
                    lines.push(format!("{} {}", &line[..=eq], render_list(user_list)));
                }
            }
            None => lines.push(format!("# {line}")),
        }
    }

    let mut out = lines.join("\n");
    if template.ends_with('\n') {
        out.push('\n');
    }
    // Anything the walk never asked about: an action this layout does not
    // name at all. It is not in the new file, and the only honest thing left
    // is to say so.
    let mut unplaced: Vec<String> = live
        .keys()
        .filter(|key| !used.contains(*key))
        .map(|(section, action)| format!("{section}.{action}"))
        .collect();
    unplaced.sort();
    Regenerated {
        text: out,
        unplaced,
    }
}

/// The binding strings inside the first `[ ... ]` on the value side of a line.
///
/// A keymap binding never contains a comma or a bracket, so splitting on commas
/// and stripping the quotes is exact for the file's `["a", "b"]` form and needs
/// no TOML parse of a single line.
fn extract_list(after_eq: &str) -> Vec<String> {
    let (Some(open), Some(close)) = (after_eq.find('['), after_eq.find(']')) else {
        return Vec::new();
    };
    if close <= open {
        return Vec::new();
    }
    after_eq[open + 1..close]
        .split(',')
        .map(|p| p.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Render a binding list the way the file writes one: `["a", "b"]`.
fn render_list(items: &[String]) -> String {
    let parts: Vec<String> = items.iter().map(|s| format!("\"{s}\"")).collect();
    format!("[{}]", parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reference_keymap_is_what_the_generator_produces() {
        // `examples/keymap.toml` IS the canonical layout, every action live.
        // The design keeps the file as the single source, so this is not the
        // struct-vs-file check `config.toml` needs but a round-trip guard: the
        // walker that regenerates a user's file must reproduce the reference
        // one byte-for-byte, or every regeneration reformats something. Run the
        // suite with `HCMD_REGEN_KEYMAP=1` after an intended layout change to
        // rewrite the file, then rebuild so the embedded copy catches up.
        let generated = generate();
        if std::env::var_os("HCMD_REGEN_KEYMAP").is_some() {
            std::fs::write("examples/keymap.toml", &generated).expect("rewrite the reference file");
        }
        assert_eq!(
            generated,
            crate::config::EXAMPLE_KEYMAP,
            "examples/keymap.toml is stale; regenerate with HCMD_REGEN_KEYMAP=1"
        );
    }

    #[test]
    fn a_set_binding_is_kept_and_the_rest_are_commented() {
        // A minimal user file that rebinds one action: that action comes back
        // live at the bindings the user gave it, and an action the file never
        // mentioned is written commented at its default so it keeps tracking
        // the built-in.
        let version = env!("CARGO_PKG_VERSION");
        let user = "[global]\ncopy = [\"ctrl+c\"]\n";
        let out = generate_preserving(user, version).text;
        assert!(
            out.contains("\ncopy              = [\"ctrl+c\"]\n"),
            "the user's binding is live and keeps the layout's alignment: {out}"
        );
        assert!(
            out.contains("\n# move              = [\"f6\"]"),
            "an untouched action is commented at its default: {out}"
        );
        assert!(
            out.contains(&format!("written by version {version}")),
            "the regenerated file is stamped: {out}"
        );
    }

    #[test]
    fn a_newly_added_action_appears_after_regeneration() {
        // The trap the whole feature closes: a user file written before an
        // action existed cannot list it, and a live file froze its owner out of
        // it. Regenerating writes the full canonical layout, so the action is
        // there - commented at its default, tracking the built-in - even though
        // the user's file never named it.
        let version = env!("CARGO_PKG_VERSION");
        let user = "[global]\ncopy = [\"ctrl+c\"]\n";
        let out = generate_preserving(user, version).text;
        assert!(
            out.contains("theme_picker"),
            "an action the user file omits is present after regeneration: {out}"
        );
    }

    #[test]
    fn regeneration_is_idempotent() {
        // A file the generator already produced regenerates to itself, so a
        // second `--update-config` is a no-op and makes no backup.
        let version = env!("CARGO_PKG_VERSION");
        let user = "[global]\ncopy = [\"ctrl+c\"]\n[panel]\nparent = [\"ctrl+pgup\"]\n";
        let once = generate_preserving(user, version).text;
        let twice = generate_preserving(&once, version).text;
        assert_eq!(once, twice, "a second regeneration changes nothing");
    }

    #[test]
    fn an_unparsable_file_yields_the_fresh_canonical_layout() {
        // A file that will not parse sets nothing, so every action is written
        // commented at its default - the fresh layout - and the caller backs
        // the broken file up dated. No binding survives, which is the honest
        // outcome when the file cannot be read.
        let version = env!("CARGO_PKG_VERSION");
        let out = generate_preserving("this is = = not toml", version).text;
        assert!(
            out.contains("\n# copy              = [\"f5\"]"),
            "every action is commented at its default: {out}"
        );
        assert!(
            !out.contains("\ncopy              ="),
            "nothing is left live: {out}"
        );
    }
}
