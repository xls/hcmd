//! Named multi-rename presets, and the file they live in.
//!
//! > Plus **Load/save settings**, bound to `F2`, for named presets.
//!
//! # Which file, and why
//!
//! the design names no file and the list of configuration files does not
//! have one. the design resolves it: `renames.toml` in the **config**
//! directory beside `hotlist.toml`, because a preset is something the user
//! curates and would want version-controlled, which is the own stated
//! criterion for that directory. the list is incomplete rather than
//! closed, and that is reported rather than guessed at.
//!
//! # The file is a mirror, not the model
//!
//! [`SavedRename`] is a flat record of strings and numbers, not a `serde`
//! derive on [`Settings`]. Two reasons: the on-disk names are then a decision
//! rather than a consequence of a field name somebody may rename later, and
//! [`Case`] stays a plain enum with no serialization attributes on it. Every
//! field has a `#[serde(default)]`, so a hand-written preset that sets only
//! `name_mask` loads, which is what a user editing this file by hand will
//! write.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::paths::config_dir;
use crate::error::{Error, Result};

use super::mask::Counter;
use super::plan::Settings;
use super::replace::{Case, Replace};

/// The file, in the config directory.
pub const RENAMES_FILE: &str = "renames.toml";

/// The most presets one file holds.
///
/// A bound rather than a promise: the `F2` list is a menu, and a menu with
/// thousands of entries in it is a different control. A hand-written file past
/// this loads its first [`MAX_PRESETS`] and says so.
pub const MAX_PRESETS: usize = 64;

/// One named preset, as it appears in `renames.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedRename {
    /// What the `F2` list calls it.
    pub name: String,
    /// the first combo.
    pub name_mask: String,
    /// Its second combo.
    pub ext_mask: String,
    /// `Search for`.
    pub search: String,
    /// `Replace with`.
    pub replace_with: String,
    /// The `1x` toggle.
    pub first_only: bool,
    /// The `[E]` toggle.
    pub include_ext: bool,
    /// The `RegEx` toggle.
    pub regex: bool,
    /// The `Subst.` toggle.
    pub substitute: bool,
    /// The `^` toggle, which is case sensitivity.
    pub match_case: bool,
    /// The Upper/lowercase dropdown, by [`Case::id`].
    pub case: String,
    /// `Start at`.
    pub counter_start: i64,
    /// `Step by`.
    pub counter_step: i64,
    /// `Digits`.
    pub counter_digits: u8,
}

impl Default for SavedRename {
    fn default() -> Self {
        Self::from_settings("", &Settings::reset())
    }
}

impl SavedRename {
    /// Snapshot the dialog's settings under a name.
    pub fn from_settings(name: impl Into<String>, settings: &Settings) -> Self {
        Self {
            name: name.into(),
            name_mask: settings.name_mask.clone(),
            ext_mask: settings.ext_mask.clone(),
            search: settings.replace.search.clone(),
            replace_with: settings.replace.with.clone(),
            first_only: settings.replace.first_only,
            include_ext: settings.replace.include_ext,
            regex: settings.replace.regex,
            substitute: settings.replace.substitute,
            match_case: settings.replace.match_case,
            case: settings.case.id().to_string(),
            counter_start: settings.counter.start,
            counter_step: settings.counter.step,
            counter_digits: settings.counter.digits,
        }
    }

    /// The settings this preset restores.
    ///
    /// An unrecognised `case` falls back to [`Case::Unchanged`] rather than
    /// refusing the whole preset: the rest of it is still what the user asked
    /// for, and the dropdown shows what it fell back to.
    pub fn settings(&self) -> Settings {
        Settings {
            name_mask: self.name_mask.clone(),
            ext_mask: self.ext_mask.clone(),
            replace: Replace {
                search: self.search.clone(),
                with: self.replace_with.clone(),
                first_only: self.first_only,
                include_ext: self.include_ext,
                regex: self.regex,
                substitute: self.substitute,
                match_case: self.match_case,
            },
            case: Case::from_id(&self.case).unwrap_or_default(),
            counter: Counter {
                start: self.counter_start,
                step: self.counter_step,
                // A width of zero pads to nothing, which is a legal counter but
                // never what a hand-written `0` meant.
                digits: self.counter_digits.max(1),
            },
        }
    }
}

/// The whole file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameFile {
    /// `[[preset]]` tables, in the order they appear.
    #[serde(default, rename = "preset")]
    pub presets: Vec<SavedRename>,
}

/// Where the file lives.
pub fn path() -> Result<PathBuf> {
    Ok(config_dir()?.join(RENAMES_FILE))
}

/// Parse the file's text.
///
/// A parse failure is not an error to the caller: it is an empty list and a
/// warning, the same degradation `panel::state` makes, because a preset file a
/// user broke by hand must not stop the program starting.
pub fn parse(text: &str) -> (Vec<SavedRename>, Vec<String>) {
    match toml::from_str::<RenameFile>(text) {
        Ok(file) => {
            let mut warnings = Vec::new();
            let mut presets = file.presets;
            if presets.len() > MAX_PRESETS {
                warnings.push(format!(
                    "{RENAMES_FILE}: {} presets; only the first {MAX_PRESETS} are offered",
                    presets.len()
                ));
                presets.truncate(MAX_PRESETS);
            }
            (presets, warnings)
        }
        Err(err) => (Vec::new(), vec![format!("{RENAMES_FILE}: {err}")]),
    }
}

/// Read the presets, degrading to none on every failure.
pub fn load() -> (Vec<SavedRename>, Vec<String>) {
    let Ok(path) = path() else {
        return (Vec::new(), Vec::new());
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text),
        // A missing file is the normal case, not a warning: most users never
        // save a preset.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (Vec::new(), Vec::new()),
        Err(err) => (Vec::new(), vec![format!("{}: {err}", path.display())]),
    }
}

/// Write the presets, creating the config directory if it is not there.
pub fn save(presets: &[SavedRename]) -> Result<()> {
    let path = path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    }
    let file = RenameFile {
        presets: presets.iter().take(MAX_PRESETS).cloned().collect(),
    };
    let text = toml::to_string_pretty(&file)
        .map_err(|e| Error::msg(format!("serializing {RENAMES_FILE}: {e}")))?;
    std::fs::write(&path, text).map_err(|e| Error::io(&path, e))
}

/// Add or replace a preset by name, newest last, and hand back the new list.
///
/// Saving under a name that is already there **replaces** it: that is what a
/// user pressing `F2` and typing the same name means, and offering two presets
/// with one name would make the list ambiguous.
pub fn upsert(presets: &[SavedRename], preset: SavedRename) -> Vec<SavedRename> {
    let mut out: Vec<SavedRename> = presets
        .iter()
        .filter(|p| p.name != preset.name)
        .cloned()
        .collect();
    out.push(preset);
    out.truncate(MAX_PRESETS);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            name_mask: "[N]_[C]".to_string(),
            ext_mask: "[E]".to_string(),
            replace: Replace {
                search: "a".to_string(),
                with: "b".to_string(),
                first_only: true,
                regex: true,
                ..Replace::default()
            },
            case: Case::EachWordCapital,
            counter: Counter {
                start: 10,
                step: 5,
                digits: 3,
            },
        }
    }

    #[test]
    fn a_preset_survives_a_round_trip_through_the_file() {
        let preset = SavedRename::from_settings("by date", &settings());
        let text = toml::to_string_pretty(&RenameFile {
            presets: vec![preset.clone()],
        })
        .expect("serializes");
        let (back, warnings) = parse(&text);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(back, vec![preset]);
        assert_eq!(
            back.first().map(SavedRename::settings),
            Some(settings()),
            "and the settings come back exactly"
        );
    }

    #[test]
    fn a_hand_written_preset_needs_only_the_field_it_cares_about() {
        // Every field defaults, so this is a legal file.
        let (presets, warnings) = parse("[[preset]]\nname = \"upper\"\ncase = \"upper\"\n");
        assert!(warnings.is_empty(), "{warnings:?}");
        let first = presets.first().expect("one preset");
        assert_eq!(first.name, "upper");
        let settings = first.settings();
        assert_eq!(
            settings.name_mask, "[N]",
            "the default mask, not an empty one"
        );
        assert_eq!(settings.ext_mask, "[E]");
        assert_eq!(settings.case, Case::Upper);
        assert_eq!(settings.counter, Counter::DEFAULT);
    }

    #[test]
    fn a_broken_file_is_a_warning_and_no_presets() {
        let (presets, warnings) = parse("[[preset]\nname = ");
        assert!(presets.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings.first().is_some_and(|w| w.contains(RENAMES_FILE)),
            "{warnings:?}"
        );
    }

    #[test]
    fn an_unknown_case_falls_back_rather_than_refusing_the_preset() {
        let (presets, _) =
            parse("[[preset]]\nname = \"x\"\ncase = \"sideways\"\nname_mask = \"[C]\"\n");
        let settings = presets.first().expect("one preset").settings();
        assert_eq!(settings.case, Case::Unchanged);
        assert_eq!(settings.name_mask, "[C]");
    }

    #[test]
    fn a_zero_digit_counter_is_read_as_one_digit() {
        let (presets, _) = parse("[[preset]]\nname = \"x\"\ncounter_digits = 0\n");
        assert_eq!(
            presets.first().map(|p| p.settings().counter.digits),
            Some(1)
        );
    }

    #[test]
    fn saving_under_an_existing_name_replaces_it() {
        let first = SavedRename::from_settings("photos", &Settings::reset());
        let second = SavedRename::from_settings("photos", &settings());
        let other = SavedRename::from_settings("docs", &Settings::reset());
        let list = upsert(&upsert(&[], first), other);
        assert_eq!(list.len(), 2);
        let list = upsert(&list, second.clone());
        assert_eq!(list.len(), 2, "one name, one preset");
        assert_eq!(list.last(), Some(&second));
    }

    #[test]
    fn too_many_presets_are_capped_with_a_warning() {
        let mut text = String::new();
        for i in 0..MAX_PRESETS + 5 {
            text.push_str(&format!("[[preset]]\nname = \"p{i}\"\n"));
        }
        let (presets, warnings) = parse(&text);
        assert_eq!(presets.len(), MAX_PRESETS);
        assert_eq!(warnings.len(), 1);
    }
}
