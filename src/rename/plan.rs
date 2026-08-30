//! The preview table's model, and every status it can flag.
//!
//! > Columns, in order: **Old name · Ext · New name · Size · Date · Location**.
//! > Sortable by any column; the sort determines counter order [...] A status
//! > column flags problems: a collision with an existing file, a collision with
//! > another renamed file, an invalid character, or a no-op. The `Start!`
//! > button is disabled while any collision stands.
//!
//! # Pure, and cheap
//!
//! [`Plan::build`] opens nothing and reads nothing.
//! It is handed the rows the panel already holds and the names each directory
//! already lists, so the "the New name column updates on every
//! keystroke in any control" costs one pass over rows that are already in
//! memory.
//!
//! # What the dialog can prove, and what it cannot
//!
//! [`RenameStatus::Exists`] is judged against the names the panel holds. For an
//! ordinary one-directory rename that is every name in the directory and is
//! exact. On a virtual listing spanning directories the dialog knows only the
//! directories the rows came from and cannot know what else is in them - so the
//! dialog blocks on what it can prove and [`crate::rename::exec`] refuses per
//! file on what it cannot, checking existence immediately before every rename.
//!
//! # A row that is vacating its own name is not a collision
//!
//! the design requires `a → b` together with `b → a` to work, and the design
//! disables `Start!` on a collision with an existing file. Read literally the
//! two cancel out: `b` exists, so `a → b` would be flagged and the swap the
//! spec demands could never be started. So `Exists` asks a narrower question -
//! *is the name still going to be there when the batch runs?* - and a name that
//! another row in the same directory is renaming away from is not. This is
//! recorded as a resolution rather than a reading; see the agent report for
//! v0.6.

use std::collections::{HashMap, HashSet};

use crate::panel::{ColumnId, compare_by, name_cmp};
use crate::vfs::{Entry, VfsPath};

use super::mask::{Context, Counter, MAX_NAME_BYTES, Mask};
use super::replace::{Case, CompiledReplace, Replace};

/// the columns, in its order, plus the status column last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewColumn {
    /// The name as it is now.
    OldName,
    /// Its extension, by [`Entry::split_name`]'s definition.
    Ext,
    /// What it would become.
    NewName,
    /// The size the panel already knows.
    Size,
    /// The mtime the panel already knows.
    Date,
    /// The directory the row really lives in.
    Location,
    /// What is wrong with the row, if anything.
    Status,
}

impl PreviewColumn {
    /// Every column, in the order they are drawn.
    pub const ALL: &'static [Self] = &[
        Self::OldName,
        Self::Ext,
        Self::NewName,
        Self::Size,
        Self::Date,
        Self::Location,
        Self::Status,
    ];

    /// The column heading.
    pub const fn header(self) -> &'static str {
        match self {
            Self::OldName => "Old name",
            Self::Ext => "Ext",
            Self::NewName => "New name",
            Self::Size => "Size",
            Self::Date => "Date",
            Self::Location => "Location",
            Self::Status => "Status",
        }
    }

    /// Whether clicking or `Ctrl+<n>`-ing this column sorts by it.
    ///
    /// False for `Status` alone: the design says "sortable by any column" and
    /// then names the status column in a separate sentence, and sorting by a
    /// status would make the counter depend on how many collisions there are.
    pub const fn sortable(self) -> bool {
        !matches!(self, Self::Status)
    }
}

/// What the status column flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameStatus {
    /// A real rename with nothing wrong with it.
    Ok,
    /// the "a no-op": the new name equals the old one. The row is
    /// skipped at execution.
    NoChange,
    /// "A collision with an existing file" - a name already in the row's own
    /// directory, as the panel listed it, that no other row is vacating.
    Exists,
    /// "A collision with another renamed file", naming the row it collides
    /// with so the table can point at both.
    Duplicate(usize),
    /// "An invalid character": `/` or NUL, or the whole name is `.` or `..`.
    InvalidChar(char),
    /// A mask that expanded to nothing.
    Empty,
    /// Longer than [`MAX_NAME_BYTES`].
    TooLong,
    /// A date placeholder on a row with no mtime. A warning, not a
    /// refusal.
    NoDate,
}

impl RenameStatus {
    /// Does this status disable `Start!`?
    ///
    /// the design names collisions. Invalid, empty and over-long names are
    /// added, because they are collisions with the filesystem and the rule is
    /// that a name that cannot work is "refused in the dialog, before
    /// anything happens". A no-op is a legitimate row and a missing date is a
    /// fact about the file, so neither of those blocks.
    pub const fn blocks(&self) -> bool {
        match self {
            Self::Ok | Self::NoChange | Self::NoDate => false,
            Self::Exists
            | Self::Duplicate(_)
            | Self::InvalidChar(_)
            | Self::Empty
            | Self::TooLong => true,
        }
    }

    /// True when this row would actually be renamed.
    ///
    /// A blocking row will not be, and a no-op does not need to be - the design
    /// calls it a no-op and contract it is skipped at execution.
    pub const fn moves(&self) -> bool {
        matches!(self, Self::Ok | Self::NoDate)
    }

    /// The status column's text. Empty for [`RenameStatus::Ok`], because the
    /// column that is empty most of the time should look it.
    pub fn label(&self) -> String {
        match self {
            Self::Ok => String::new(),
            Self::NoChange => "no change".to_string(),
            Self::Exists => "exists".to_string(),
            // 1-based, because the table's rows are counted from 1 on screen.
            Self::Duplicate(row) => format!("dup of {}", row.saturating_add(1)),
            Self::InvalidChar(c) if *c == '\0' => "NUL in name".to_string(),
            Self::InvalidChar(c) => format!("bad char {c}"),
            Self::Empty => "empty name".to_string(),
            Self::TooLong => "too long".to_string(),
            Self::NoDate => "no date".to_string(),
        }
    }
}

/// One row of the preview.
#[derive(Debug, Clone)]
pub struct RenameItem {
    /// The row as the panel had it, so the Size and Date columns cost nothing.
    pub entry: Entry,
    /// Its real address, honouring [`Entry::location`].
    pub from: VfsPath,
    /// The directory it lives in, which is what a collision is judged against.
    pub dir: VfsPath,
    /// What the four control groups made of its name.
    pub new_name: String,
    /// What, if anything, is wrong with that.
    pub status: RenameStatus,
}

impl RenameItem {
    /// The address it would move to. `None` when the status blocks.
    pub fn to(&self) -> Option<VfsPath> {
        (!self.status.blocks()).then(|| self.dir.join(&self.new_name))
    }
}

/// Everything the four control groups add up to.
///
/// [`Settings::default`] is [`Settings::reset`] rather than a derived default:
/// an empty name mask expands to an empty name for every row, and a type whose
/// default state refuses every rename is a trap for the first caller who writes
/// `Settings::default()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The name mask, as typed. `[N]` by default.
    pub name_mask: String,
    /// The extension mask, as typed. `[E]` by default.
    pub ext_mask: String,
    /// Search and replace, with its five toggles.
    pub replace: Replace,
    /// The Upper/lowercase dropdown.
    pub case: Case,
    /// `Define counter [C]`.
    pub counter: Counter,
}

impl Settings {
    /// the reset control: masks back to `[N]` / `[E]`, everything
    /// else to its default.
    pub fn reset() -> Self {
        Self {
            name_mask: "[N]".to_string(),
            ext_mask: "[E]".to_string(),
            replace: Replace::default(),
            case: Case::default(),
            counter: Counter::DEFAULT,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self::reset()
    }
}

/// The whole preview: the rows, in the table's current order, with their
/// statuses already decided.
#[derive(Debug, Clone)]
pub struct Plan {
    items: Vec<RenameItem>,
    /// What each directory already holds, so a re-sort does not need it again.
    siblings: HashMap<VfsPath, HashSet<String>>,
}

impl Plan {
    /// Build one.
    ///
    /// **Pure**: it is handed the rows and the names already in each directory,
    /// and reads nothing.
    ///
    /// `rows` are `(entry, its real address)`, in the table's current order -
    /// which is what decides the counter. `siblings` is the set of names each
    /// directory already holds; for the ordinary one-directory case that is
    /// `Tab::entries`, free and already in memory. A directory not in the map
    /// contributes no [`RenameStatus::Exists`], and contract what catches those
    /// instead.
    pub fn build(
        rows: Vec<(Entry, VfsPath)>,
        settings: &Settings,
        siblings: &HashMap<VfsPath, HashSet<String>>,
    ) -> Self {
        let items = rows
            .into_iter()
            .map(|(entry, from)| {
                let dir = from.parent().unwrap_or_else(|| from.clone());
                RenameItem {
                    entry,
                    from,
                    dir,
                    new_name: String::new(),
                    status: RenameStatus::Ok,
                }
            })
            .collect();
        let mut plan = Self {
            items,
            siblings: siblings.clone(),
        };
        plan.recompute(settings);
        plan
    }

    /// The rows, in the table's current order.
    pub fn items(&self) -> &[RenameItem] {
        &self.items
    }

    /// Re-sort, which **renumbers the counter**.
    ///
    /// That renumbering is the whole point of the sentence: "the sort
    /// determines counter order, which matters - this is how 'number these by
    /// date' is expressed". Sorting by a column that is not
    /// [`PreviewColumn::sortable`] does nothing at all.
    pub fn sort(&mut self, column: PreviewColumn, reverse: bool, settings: &Settings) {
        if !column.sortable() {
            return;
        }
        self.items.sort_by(|a, b| {
            let order = match column {
                PreviewColumn::OldName => name_cmp(&a.entry.name, &b.entry.name),
                PreviewColumn::Ext => compare_by(ColumnId::Ext, &a.entry, &b.entry),
                PreviewColumn::NewName => name_cmp(&a.new_name, &b.new_name),
                PreviewColumn::Size => compare_by(ColumnId::Size, &a.entry, &b.entry),
                PreviewColumn::Date => compare_by(ColumnId::Date, &a.entry, &b.entry),
                PreviewColumn::Location => name_cmp(&a.dir.to_string(), &b.dir.to_string()),
                // Refused above; an arm rather than a wildcard so a new column
                // cannot be forgotten here.
                PreviewColumn::Status => std::cmp::Ordering::Equal,
            };
            // The name is the tiebreak, so a sort by size or date is stable in
            // the way the panel's is.
            let order = order.then_with(|| name_cmp(&a.entry.name, &b.entry.name));
            if reverse { order.reverse() } else { order }
        });
        self.recompute(settings);
    }

    /// True when any row blocks, which is what disables `Start!`.
    pub fn blocked(&self) -> bool {
        self.items.iter().any(|i| i.status.blocks())
    }

    /// The first blocking status, for the line under the table.
    pub fn first_problem(&self) -> Option<(usize, &RenameStatus)> {
        self.items
            .iter()
            .enumerate()
            .find(|(_, i)| i.status.blocks())
            .map(|(index, item)| (index, &item.status))
    }

    /// How many rows would actually move.
    pub fn changes(&self) -> usize {
        self.items.iter().filter(|i| i.status.moves()).count()
    }

    /// The pairs a job would be given, source order preserved.
    ///
    /// No-ops and blocked rows are not in it: a job that renamed a file to its
    /// own name would be a rename onto a path that exists, which
    /// [`crate::rename::exec`] refuses on principle.
    pub fn pairs(&self) -> Vec<(VfsPath, VfsPath)> {
        self.items
            .iter()
            .filter(|i| i.status.moves())
            .filter_map(|i| i.to().map(|to| (i.from.clone(), to)))
            .collect()
    }

    /// Expand every row and decide every status, in the current order.
    ///
    /// The pipeline is contract the, in that order: name mask, extension
    /// mask, search and replace, case, join. Replace runs **after** the masks
    /// because the masks build the candidate and the replacement corrects it;
    /// the other order would let `[N]` re-introduce exactly what was
    /// replaced.
    fn recompute(&mut self, settings: &Settings) {
        let name_mask = Mask::parse(&settings.name_mask);
        let ext_mask = Mask::parse(&settings.ext_mask);
        let wants_date = name_mask.uses_date() || ext_mask.uses_date();
        // Compiled once for the whole table, not once per row: a bad pattern
        // is reported by the dialog and leaves the names as the masks made
        // them, which is what the user is still typing.
        let replace: Option<CompiledReplace> = if settings.replace.is_empty() {
            None
        } else {
            settings.replace.compile().ok()
        };

        for (index, item) in self.items.iter_mut().enumerate() {
            let (stem, ext) = item.entry.split_name();
            let counter = settings.counter.value(index);
            // The parent of the row's **real home**, not the panel's path: on
            // a search result those differ and it is the whole point of
            // `[P]`. A root directory has no name, and expands to nothing
            // rather than to a separator.
            let parent = item.dir.file_name().unwrap_or_default();
            let ctx = Context {
                stem,
                ext,
                parent: &parent,
                // Local time, matching the `date` column.
                mtime: item
                    .entry
                    .mtime
                    .map(chrono::DateTime::<chrono::Local>::from),
                counter: &counter,
            };
            let mut new_stem = name_mask.expand(&ctx);
            let mut new_ext = ext_mask.expand(&ctx);
            if let Some(replace) = replace.as_ref() {
                new_stem = replace.apply(&new_stem);
                if settings.replace.include_ext {
                    new_ext = replace.apply(&new_ext);
                }
            }
            new_stem = settings.case.apply(&new_stem);
            new_ext = settings.case.apply(&new_ext);
            // No dot when the extension expands empty, so `[E]` on a file
            // without one does not produce a trailing dot.
            item.new_name = if new_ext.is_empty() {
                new_stem
            } else {
                format!("{new_stem}.{new_ext}")
            };
        }

        self.decide(wants_date);
    }

    /// Decide every row's status, once the new names are all known.
    ///
    /// One pass to index the batch by directory, then one pass per row against
    /// that index. The obvious two nested loops are `O(n^2)` and the design
    /// rebuilds this on every keystroke, so ten thousand rows would be seconds
    /// rather than a frame.
    fn decide(&mut self, wants_date: bool) {
        let statuses = self.statuses(wants_date);
        for (item, status) in self.items.iter_mut().zip(statuses) {
            item.status = status;
        }
    }

    /// Every row's status, in order.
    fn statuses(&self, wants_date: bool) -> Vec<RenameStatus> {
        let mut index: HashMap<&VfsPath, DirIndex<'_>> = HashMap::new();
        for (row, item) in self.items.iter().enumerate() {
            let dir = index.entry(&item.dir).or_default();
            if item.new_name == item.entry.name {
                dir.kept.insert(item.entry.name.as_str());
                continue;
            }
            dir.vacated.insert(item.entry.name.as_str());
            dir.producers
                .entry(item.new_name.as_str())
                .and_modify(|(_, second)| {
                    if second.is_none() {
                        *second = Some(row);
                    }
                })
                .or_insert((row, None));
        }

        self.items
            .iter()
            .enumerate()
            .map(|(row, item)| self.status_of(row, item, wants_date, index.get(&item.dir)))
            .collect()
    }

    /// One row's status. Split out so the order the tests below assert is the
    /// order written here, once.
    fn status_of(
        &self,
        row: usize,
        item: &RenameItem,
        wants_date: bool,
        dir: Option<&DirIndex<'_>>,
    ) -> RenameStatus {
        // A name that cannot work at all comes first: it is true whatever the
        // rest of the batch does.
        if item.new_name.is_empty() {
            return RenameStatus::Empty;
        }
        if let Some(c) = item.new_name.chars().find(|c| *c == '/' || *c == '\0') {
            return RenameStatus::InvalidChar(c);
        }
        if item.new_name == "." || item.new_name == ".." {
            return RenameStatus::InvalidChar('.');
        }
        if item.new_name.len() > MAX_NAME_BYTES {
            return RenameStatus::TooLong;
        }
        // Then the no-op, before the collisions: a row that is not moving
        // cannot collide with anything, least of all with itself.
        if item.new_name == item.entry.name {
            return RenameStatus::NoChange;
        }
        // A name another row is vacating will be free by the time the batch
        // runs; a name another row is *keeping* will not, and that is provable
        // from the rows alone even where `siblings` knows nothing.
        let name = item.new_name.as_str();
        let vacated = dir.is_some_and(|d| d.vacated.contains(name));
        let kept_by_row = dir.is_some_and(|d| d.kept.contains(name));
        let listed = self
            .siblings
            .get(&item.dir)
            .is_some_and(|names| names.contains(&item.new_name));
        if kept_by_row || (listed && !vacated) {
            return RenameStatus::Exists;
        }
        // Judged per directory: on a virtual listing the rows come from many,
        // and two rows in different directories producing `report.txt` do not
        // collide. The row it names is the *other* one, so the table can point
        // at both.
        if let Some((first, second)) = dir.and_then(|d| d.producers.get(name)).copied() {
            let other = if first == row { second } else { Some(first) };
            if let Some(other) = other {
                return RenameStatus::Duplicate(other);
            }
        }
        if wants_date && item.entry.mtime.is_none() {
            return RenameStatus::NoDate;
        }
        RenameStatus::Ok
    }
}

/// What one directory's rows add up to, so a status costs a hash lookup rather
/// than a pass over the batch.
#[derive(Debug, Default)]
struct DirIndex<'a> {
    /// Old names of rows that are moving away, and whose names will be free.
    vacated: HashSet<&'a str>,
    /// Old names of rows that are staying put, and whose names will not be.
    kept: HashSet<&'a str>,
    /// New name to the first two rows that produce it, moving rows only.
    producers: HashMap<&'a str, (usize, Option<usize>)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn dir() -> VfsPath {
        VfsPath::local("/srv/media")
    }

    fn file(name: &str) -> Entry {
        let mut entry = Entry::file(name);
        entry.size = 10;
        entry.mtime = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_787_000_000));
        entry
    }

    fn rows(names: &[&str]) -> Vec<(Entry, VfsPath)> {
        names.iter().map(|n| (file(n), dir().join(n))).collect()
    }

    fn siblings(names: &[&str]) -> HashMap<VfsPath, HashSet<String>> {
        let mut map = HashMap::new();
        map.insert(
            dir(),
            names
                .iter()
                .map(|n| (*n).to_string())
                .collect::<HashSet<_>>(),
        );
        map
    }

    fn settings(name_mask: &str) -> Settings {
        Settings {
            name_mask: name_mask.to_string(),
            ..Settings::reset()
        }
    }

    fn names(plan: &Plan) -> Vec<String> {
        plan.items().iter().map(|i| i.new_name.clone()).collect()
    }

    #[test]
    fn the_default_settings_are_the_reset_settings() {
        // An empty name mask would refuse every rename, so the derived default
        // is deliberately not the one this type has.
        assert_eq!(Settings::default(), Settings::reset());
        assert_eq!(Settings::reset().name_mask, "[N]");
        assert_eq!(Settings::reset().ext_mask, "[E]");
    }

    #[test]
    fn the_default_masks_leave_every_name_exactly_as_it_was() {
        let plan = Plan::build(
            rows(&["a.txt", "b", ".bashrc", "archive.tar.gz"]),
            &Settings::reset(),
            &siblings(&["a.txt", "b", ".bashrc", "archive.tar.gz"]),
        );
        assert_eq!(
            names(&plan),
            vec!["a.txt", "b", ".bashrc", "archive.tar.gz"]
        );
        assert!(
            plan.items()
                .iter()
                .all(|i| i.status == RenameStatus::NoChange),
            "{:?}",
            plan.items().iter().map(|i| &i.status).collect::<Vec<_>>()
        );
        assert_eq!(plan.changes(), 0);
        assert!(plan.pairs().is_empty());
        assert!(!plan.blocked());
    }

    #[test]
    fn the_counter_numbers_the_table_in_its_current_order() {
        // Acceptance criterion 9: four files, `[N]_[C]`, start 10, step 5,
        // three digits.
        let set = Settings {
            name_mask: "[N]_[C]".to_string(),
            ext_mask: String::new(),
            counter: Counter {
                start: 10,
                step: 5,
                digits: 3,
            },
            ..Settings::reset()
        };
        let plan = Plan::build(rows(&["a", "a", "a", "a"]), &set, &siblings(&["a"]));
        assert_eq!(names(&plan), vec!["a_010", "a_015", "a_020", "a_025"]);
        assert_eq!(plan.changes(), 4);
    }

    #[test]
    fn re_sorting_renumbers_the_counter() {
        // "the sort determines counter order, which matters -
        // this is how 'number these by date' is expressed".
        let set = Settings {
            name_mask: "[C]-[N]".to_string(),
            ext_mask: String::new(),
            ..Settings::reset()
        };
        let mut plan = Plan::build(rows(&["c", "a", "b"]), &set, &siblings(&[]));
        assert_eq!(names(&plan), vec!["1-c", "2-a", "3-b"]);
        plan.sort(PreviewColumn::OldName, false, &set);
        assert_eq!(names(&plan), vec!["1-a", "2-b", "3-c"]);
        plan.sort(PreviewColumn::OldName, true, &set);
        assert_eq!(names(&plan), vec!["1-c", "2-b", "3-a"]);
        // The status column does not sort, so it does not renumber either.
        plan.sort(PreviewColumn::Status, false, &set);
        assert_eq!(names(&plan), vec!["1-c", "2-b", "3-a"]);
        assert!(!PreviewColumn::Status.sortable());
        assert!(PreviewColumn::ALL.iter().filter(|c| c.sortable()).count() == 6);
    }

    #[test]
    fn the_pipeline_runs_the_masks_before_the_replacement() {
        // the masks build the candidate and the replacement corrects it.
        // Replacing `report` before `[N]` expanded would put it straight
        // back.
        let set = Settings {
            name_mask: "[N]".to_string(),
            ext_mask: "[E]".to_string(),
            replace: Replace {
                search: "report".to_string(),
                with: "summary".to_string(),
                ..Replace::default()
            },
            ..Settings::reset()
        };
        let plan = Plan::build(rows(&["report.txt"]), &set, &siblings(&[]));
        assert_eq!(names(&plan), vec!["summary.txt"]);
    }

    #[test]
    fn the_replacement_reaches_the_extension_only_behind_the_e_toggle() {
        let base = Replace {
            search: "t".to_string(),
            with: "T".to_string(),
            ..Replace::default()
        };
        let off = Settings {
            replace: base.clone(),
            ..Settings::reset()
        };
        let plan = Plan::build(rows(&["ttt.txt"]), &off, &siblings(&[]));
        assert_eq!(names(&plan), vec!["TTT.txt"]);

        let on = Settings {
            replace: Replace {
                include_ext: true,
                ..base
            },
            ..Settings::reset()
        };
        let plan = Plan::build(rows(&["ttt.txt"]), &on, &siblings(&[]));
        assert_eq!(names(&plan), vec!["TTT.TxT"]);
    }

    #[test]
    fn the_case_dropdown_reaches_the_extension_too() {
        // `UPPER` produces `README.TXT`, not `README.txt`.
        let set = Settings {
            case: Case::Upper,
            ..Settings::reset()
        };
        let plan = Plan::build(rows(&["readme.txt"]), &set, &siblings(&[]));
        assert_eq!(names(&plan), vec!["README.TXT"]);
    }

    #[test]
    fn an_empty_extension_adds_no_trailing_dot() {
        // `[E]` on a file with no extension, and an extension
        // mask cleared by hand.
        let plan = Plan::build(rows(&["plain"]), &Settings::reset(), &siblings(&[]));
        assert_eq!(names(&plan), vec!["plain"]);
        let set = Settings {
            ext_mask: String::new(),
            ..Settings::reset()
        };
        let plan = Plan::build(rows(&["a.txt"]), &set, &siblings(&[]));
        assert_eq!(names(&plan), vec!["a"]);
    }

    #[test]
    fn every_collision_class_is_flagged_and_every_blocking_one_blocks() {
        // Contract the table, row by row. `blocks()` is the whole of
        // "the `Start!` button is disabled while any collision stands".
        let all = [
            RenameStatus::Ok,
            RenameStatus::NoChange,
            RenameStatus::Exists,
            RenameStatus::Duplicate(1),
            RenameStatus::InvalidChar('/'),
            RenameStatus::Empty,
            RenameStatus::TooLong,
            RenameStatus::NoDate,
        ];
        let blocking: Vec<bool> = all.iter().map(RenameStatus::blocks).collect();
        assert_eq!(
            blocking,
            vec![false, false, true, true, true, true, true, false]
        );
        // A blocking row never yields a target, so nothing downstream can act
        // on one by accident.
        for status in all {
            let item = RenameItem {
                entry: file("a"),
                from: dir().join("a"),
                dir: dir(),
                new_name: "b".to_string(),
                status: status.clone(),
            };
            assert_eq!(item.to().is_none(), status.blocks(), "{status:?}");
            assert!(!status.blocks() || !status.moves());
        }
    }

    #[test]
    fn a_collision_with_an_existing_file_blocks_and_a_changed_mask_clears_it() {
        // Acceptance criterion 11.
        let set = settings("taken");
        let mut plan = Plan::build(rows(&["a.txt"]), &set, &siblings(&["a.txt", "taken.txt"]));
        assert_eq!(
            plan.items().first().map(|i| &i.status),
            Some(&RenameStatus::Exists)
        );
        assert!(plan.blocked());
        assert_eq!(plan.first_problem().map(|(i, _)| i), Some(0));

        let set = settings("free");
        plan = Plan::build(rows(&["a.txt"]), &set, &siblings(&["a.txt", "taken.txt"]));
        assert_eq!(
            plan.items().first().map(|i| &i.status),
            Some(&RenameStatus::Ok)
        );
        assert!(!plan.blocked());
        assert_eq!(plan.first_problem(), None);
    }

    #[test]
    fn a_swap_is_not_a_collision_with_the_file_it_is_swapping_with() {
        // the design requires `a -> b` with `b -> a` to work, so neither row
        // may be flagged `Exists` against a name the other is vacating.
        let set = Settings {
            name_mask: "[N2-][N1]".to_string(),
            ext_mask: String::new(),
            ..Settings::reset()
        };
        let plan = Plan::build(rows(&["ab", "ba"]), &set, &siblings(&["ab", "ba"]));
        assert_eq!(names(&plan), vec!["ba", "ab"]);
        assert!(!plan.blocked(), "{:?}", plan.items());
        assert_eq!(plan.changes(), 2);

        // But a name a *no-op* row is keeping is still occupied.
        let set = settings("b");
        let plan = Plan::build(rows(&["a", "b"]), &set, &siblings(&["a", "b"]));
        assert_eq!(
            plan.items()
                .iter()
                .map(|i| i.status.clone())
                .collect::<Vec<_>>(),
            vec![RenameStatus::Exists, RenameStatus::NoChange]
        );
        assert!(plan.blocked());
    }

    #[test]
    fn two_rows_producing_one_name_are_duplicates_of_each_other() {
        let set = settings("same");
        let plan = Plan::build(rows(&["a", "b", "c"]), &set, &siblings(&[]));
        assert_eq!(
            plan.items()
                .iter()
                .map(|i| i.status.clone())
                .collect::<Vec<_>>(),
            vec![
                RenameStatus::Duplicate(1),
                RenameStatus::Duplicate(0),
                RenameStatus::Duplicate(0),
            ],
            "each row names one it collides with, so the table can point at both"
        );
        assert!(plan.blocked());
        assert_eq!(RenameStatus::Duplicate(1).label(), "dup of 2");
    }

    #[test]
    fn duplicates_are_judged_per_directory() {
        // on a virtual listing the rows come from many directories, and two
        // `report.txt` in different ones do not collide.
        let other = VfsPath::local("/srv/backup");
        let set = settings("report");
        let rows = vec![
            (file("a.txt"), dir().join("a.txt")),
            (file("b.txt"), other.join("b.txt")),
        ];
        let plan = Plan::build(rows, &set, &HashMap::new());
        assert!(!plan.blocked(), "{:?}", plan.items());
        assert_eq!(names(&plan), vec!["report.txt", "report.txt"]);
        assert_eq!(
            plan.items()
                .iter()
                .map(|i| i.dir.clone())
                .collect::<Vec<_>>(),
            vec![dir(), other]
        );
    }

    #[test]
    fn an_invalid_character_an_empty_name_and_an_over_long_name_all_refuse() {
        let plan = Plan::build(rows(&["a"]), &settings("x/y"), &siblings(&[]));
        assert_eq!(
            plan.items().first().map(|i| i.status.clone()),
            Some(RenameStatus::InvalidChar('/'))
        );

        let plan = Plan::build(rows(&["a"]), &settings(".."), &siblings(&[]));
        assert_eq!(
            plan.items().first().map(|i| i.status.clone()),
            Some(RenameStatus::InvalidChar('.'))
        );

        let empty = Settings {
            name_mask: String::new(),
            ext_mask: String::new(),
            ..Settings::reset()
        };
        let plan = Plan::build(rows(&["a"]), &empty, &siblings(&[]));
        assert_eq!(
            plan.items().first().map(|i| i.status.clone()),
            Some(RenameStatus::Empty)
        );

        let long = Settings {
            name_mask: "x".repeat(MAX_NAME_BYTES + 1),
            ext_mask: String::new(),
            ..Settings::reset()
        };
        let plan = Plan::build(rows(&["a"]), &long, &siblings(&[]));
        assert_eq!(
            plan.items().first().map(|i| i.status.clone()),
            Some(RenameStatus::TooLong)
        );
        // And exactly 255 bytes is allowed, because the bound is the kernel's.
        let edge = Settings {
            name_mask: "x".repeat(MAX_NAME_BYTES),
            ext_mask: String::new(),
            ..Settings::reset()
        };
        let plan = Plan::build(rows(&["a"]), &edge, &siblings(&[]));
        assert_eq!(
            plan.items().first().map(|i| i.status.clone()),
            Some(RenameStatus::Ok)
        );
    }

    #[test]
    fn a_date_mask_on_a_row_with_no_mtime_warns_without_refusing() {
        // `NoDate` is a warning, and the row still renames.
        let mut entry = file("a.txt");
        entry.mtime = None;
        let set = Settings {
            name_mask: "[YMD]-[N]".to_string(),
            ..Settings::reset()
        };
        let plan = Plan::build(vec![(entry, dir().join("a.txt"))], &set, &HashMap::new());
        assert_eq!(names(&plan), vec!["-a.txt"]);
        assert_eq!(
            plan.items().first().map(|i| i.status.clone()),
            Some(RenameStatus::NoDate)
        );
        assert!(!plan.blocked());
        assert_eq!(plan.changes(), 1);

        // A mask that never asked for a date does not flag one.
        let mut entry = file("b.txt");
        entry.mtime = None;
        let plan = Plan::build(
            vec![(entry, dir().join("b.txt"))],
            &settings("c"),
            &HashMap::new(),
        );
        assert_eq!(
            plan.items().first().map(|i| i.status.clone()),
            Some(RenameStatus::Ok)
        );
    }

    #[test]
    fn the_pairs_a_job_is_given_skip_no_ops_and_blocked_rows() {
        // One row changes and one does not, so the job is handed one pair:
        // renaming a file to its own name would be a rename onto a path that
        // exists, which `rename::exec` refuses on principle.
        let set = Settings {
            replace: Replace {
                search: "a".to_string(),
                with: "z".to_string(),
                ..Replace::default()
            },
            ..Settings::reset()
        };
        let plan = Plan::build(rows(&["a", "b"]), &set, &siblings(&["a", "b"]));
        assert_eq!(names(&plan), vec!["z", "b"]);
        assert_eq!(
            plan.items()
                .iter()
                .map(|i| i.status.clone())
                .collect::<Vec<_>>(),
            vec![RenameStatus::Ok, RenameStatus::NoChange]
        );
        assert_eq!(
            plan.pairs(),
            vec![(dir().join("a"), dir().join("z"))],
            "the no-op is not handed to the job"
        );
        assert_eq!(plan.changes(), 1);
        assert!(!plan.blocked());
    }

    #[test]
    fn the_preview_reads_nothing() {
        // The paths are deliberately absurd: nothing here
        // exists, and building a plan over them still works.
        let rows = vec![(
            file("x"),
            VfsPath::local("/definitely/not/a/real/directory/x"),
        )];
        let plan = Plan::build(rows, &Settings::reset(), &HashMap::new());
        assert_eq!(names(&plan), vec!["x"]);
    }

    #[test]
    fn rebuilding_ten_thousand_rows_is_a_keystroke() {
        // the design rebuilds this on every keystroke, so it has to be a
        // pass over memory rather than anything else.
        let names: Vec<String> = (0..10_000).map(|i| format!("file{i:05}.txt")).collect();
        let rows: Vec<(Entry, VfsPath)> = names.iter().map(|n| (file(n), dir().join(n))).collect();
        let set = Settings {
            name_mask: "[YMD]_[N]_[C]".to_string(),
            ..Settings::reset()
        };
        let started = std::time::Instant::now();
        let plan = Plan::build(rows, &set, &siblings(&[]));
        let elapsed = started.elapsed();
        assert_eq!(plan.items().len(), 10_000);
        // A ceiling with room to spare. Ten thousand rows is milliseconds of
        // work; this exists to catch an accidental quadratic, not to measure
        // the machine, so the bound is set where only a change in complexity
        // can cross it. A tight one measures the runner's load instead.
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "ten thousand rows took {elapsed:?}, which is a change in shape"
        );
    }
}
