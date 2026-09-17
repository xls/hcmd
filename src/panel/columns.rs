//! Column width allocation.
//!
//! Widths are **percentages of the panel's inner width**, not fixed character
//! counts: the two panels are resizable and a fixed-column layout looks wrong at
//! every size but one.
//!
//! The algorithm is a loop, not a single pass, and [`allocate`] implements it
//! literally:
//!
//! 1. Each configured column gets `round(pct × inner_width)` characters,
//!    **clamped up** to its `min_chars`.
//! 2. `name` takes everything left over and is never dropped.
//! 3. If `name` would fall below `panel.name_min_width`, hide the first
//!    still-visible column in `hide_priority` and **start again**. Repeat until
//!    `name` fits.
//! 4. A column that cannot reach `min_chars` without squeezing `name` below its
//!    minimum is hidden by the same rule - a half-rendered date is worse than no
//!    date.
//!
//! Hiding a column changes nothing about sorting: `Ctrl+<n>` addresses the n-th
//! column in the configured `order`, drawn or not.

use crate::config::{AttrStyle, NameTruncate, PanelConfig};
use crate::panel::ColumnId;
use crate::panel::text::Align;
use crate::panel::text::Crop;

/// One space between adjacent columns, and it has to be paid for out of the
/// inner width before `name` takes the leftover.
pub const SEPARATOR: &str = " ";

/// The width of [`SEPARATOR`].
pub const SEPARATOR_WIDTH: usize = 1;

/// A column that survived allocation, with the number of cells it gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Allocated {
    /// Which column.
    pub id: ColumnId,
    /// Its width in terminal cells. Never zero.
    pub width: usize,
    /// How its cells sit. Decided here, once, from the column's kind or from
    /// the plan that defined it, so the renderer has one answer for every
    /// column and no plan to consult.
    pub align: Align,
}

/// The result of laying out one panel's columns at one width.
///
/// Recomputed on every layout, because `inner_width` changes on every resize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allocation {
    columns: Vec<Allocated>,
    inner_width: usize,
    crop: Crop,
}

impl Allocation {
    /// The visible columns, left to right in the configured order.
    pub fn columns(&self) -> &[Allocated] {
        &self.columns
    }

    /// The inner width this allocation was computed for.
    pub const fn inner_width(&self) -> usize {
        self.inner_width
    }

    /// How an over-long name is cropped in this layout.
    ///
    /// With `name_truncate = "auto"` this is decided from what is *actually
    /// rendered*: [`Crop::End`] while `ext` is visible, [`Crop::Middle`] once it
    /// is hidden. A panel that narrows past the point where `ext` drops
    /// therefore switches to middle-cropping on its own.
    pub const fn crop(&self) -> Crop {
        self.crop
    }

    /// How many cells a column got, or `None` when it is hidden.
    pub fn width_of(&self, id: ColumnId) -> Option<usize> {
        self.columns.iter().find(|c| c.id == id).map(|c| c.width)
    }

    /// Whether a column is drawn at this width.
    pub fn is_visible(&self, id: ColumnId) -> bool {
        self.columns.iter().any(|c| c.id == id)
    }

    /// The `name` column's width. Zero only for a panel with no interior at all.
    pub fn name_width(&self) -> usize {
        self.width_of(ColumnId::Name).unwrap_or(0)
    }

    /// Cells consumed by the columns plus the separators between them.
    ///
    /// Invariant, asserted in the tests at every width from 0 to 200:
    /// `total_width() <= inner_width()`.
    pub fn total_width(&self) -> usize {
        let fields: usize = self.columns.iter().map(|c| c.width).sum();
        let separators = self
            .columns
            .len()
            .saturating_sub(1)
            .saturating_mul(SEPARATOR_WIDTH);
        fields.saturating_add(separators)
    }
}

/// The default `min_chars` for a column the configuration does not mention.
///
/// Without this a column added to `order` but not to `min_chars` would allocate
/// zero cells and be silently dropped for ever, which is not what "column set
/// and order are configuration" should mean. This is not a rare path: a
/// `min_chars` table in `config.toml` *replaces* the compiled-in one rather
/// than merging with it, so a user who tunes three columns leaves every other
/// column on these values.
///
/// They therefore have to agree with [`ColumnsConfig::default`], and for `attr`
/// that means asking the configured `attr_style`: the default `"unix"` renders
/// `drwxr-xr-x`, ten cells, and a floor of four would keep the column and draw
/// it as `drwxr…` - the half-rendered column the design rules out.
///
/// [`ColumnsConfig::default`]: crate::config::ColumnsConfig::default
fn default_min_chars(cfg: &PanelConfig, id: ColumnId) -> usize {
    match id {
        // `name` is the flexible column; its floor is `panel.name_min_width`.
        ColumnId::Name => 1,
        ColumnId::Ext => 3,
        ColumnId::Size => 7,
        // "2026-08-12 02:40" is sixteen cells.
        ColumnId::Date => 16,
        ColumnId::Attr => match cfg.attr_style {
            // "drwxr-xr-x".
            AttrStyle::Unix => 10,
            // "-a--".
            AttrStyle::Dos => 4,
        },
        ColumnId::Owner | ColumnId::Group => 8,
        // "0644".
        ColumnId::PermsOctal => 4,
        // One glyph.
        ColumnId::GitState => 1,
        // Answered by the plan that defined it, in `requested_width`; this is
        // only the floor for a plan that forgot to say.
        ColumnId::Custom(_) => 8,
    }
}

/// Step 1: `round(pct × inner_width)`, clamped up to `min_chars`.
fn requested_width(
    cfg: &PanelConfig,
    id: ColumnId,
    inner_width: usize,
    plan: Option<&ColumnPlan>,
) -> usize {
    // A column the listing defined is as wide as the listing said, and takes
    // no share of the width: `name` absorbs the rest, as it always has.
    if let Some(custom) = plan.and_then(|p| p.custom(id)) {
        return usize::from(custom.min_chars);
    }
    let pct = usize::from(cfg.columns.width.get(&id).copied().unwrap_or(0));
    let min = cfg
        .columns
        .min_chars
        .get(&id)
        .map_or_else(|| default_min_chars(cfg, id), |m| usize::from(*m));
    // Integer round-half-up of pct% of inner_width, with no floating point.
    let raw = pct
        .saturating_mul(inner_width)
        .saturating_add(50)
        .saturating_div(100);
    raw.max(min)
}

/// A column a listing defines for itself, which the panel has never heard of.
///
/// A database table's `firstname` is not a column any filesystem has, so there
/// is no [`ColumnId`] for it. The listing describes it here - what to call it,
/// how its cells sit, how narrow it may go - and each row carries its value in
/// [`crate::vfs::Entry::cells`] at the same index. The panel draws, widens,
/// hides and sorts it exactly as it does `size`, and never learns the name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomColumn {
    /// What the header row says.
    pub header: String,
    /// How the cells sit. Numbers read right-aligned, everything else left.
    pub align: Align,
    /// The narrowest this column is drawn before it is dropped instead.
    pub min_chars: u16,
}

/// The columns a listing asks for, in place of the user's configured set.
///
/// A backend that knows its rows better than the configuration can returns one
/// of these and gets exactly those columns: a commit's changed files have no
/// extension worth a column of its own and no permissions at all, and the room
/// is better spent on the path. `columns` may name the panel's own kinds, or
/// [`ColumnId::Custom`] entries that index into `custom` for their definition.
/// Everything downstream is untouched - widths, the hide-by-priority order,
/// the name minimum and the redraw all work as they already do, so a listing
/// composes its columns without knowing anything about rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnPlan {
    /// The columns, in the order they are drawn and the order `Ctrl+<n>`
    /// addresses them.
    pub columns: Vec<ColumnId>,
    /// The definitions behind each [`ColumnId::Custom`] in `columns`.
    pub custom: Vec<CustomColumn>,
    /// The [`ColumnId::Name`] column's own header and width, when a listing
    /// calls its first column something other than "Name": a database's `id`,
    /// which is what the row's name really is. `None` keeps "Name" and the
    /// panel's own `name_min_width`.
    pub name: Option<CustomColumn>,
    /// Which column absorbs the leftover width and is never dropped. The Name
    /// column by default; a listing whose Name is a short id nominates a data
    /// column instead, so the id stays as narrow as it needs while the text
    /// column takes the room. A `Custom` id the plan does not draw falls back
    /// to Name. Ignored when [`ColumnPlan::pack`] is set.
    pub flex: Option<ColumnId>,
    /// Pack the columns to their own widths and leave the leftover empty,
    /// rather than stretching one column across it. A filesystem panel has one
    /// long column - the name - and fixed short ones, so stretching the name
    /// is right; a database table is a grid of columns that are each already
    /// as wide as they need to be, and stretching any one of them across the
    /// panel is the gap this avoids. The Name column is still never dropped.
    pub pack: bool,
}

impl ColumnPlan {
    /// A plan over the panel's own columns only.
    #[must_use]
    pub fn builtin(columns: Vec<ColumnId>) -> Self {
        Self {
            columns,
            custom: Vec::new(),
            name: None,
            flex: None,
            pack: false,
        }
    }

    /// The definition behind a column, if the plan gave it one - a `Custom`
    /// entry, or the Name column when the plan renamed it.
    #[must_use]
    pub fn custom(&self, id: ColumnId) -> Option<&CustomColumn> {
        match id {
            ColumnId::Custom(n) => self.custom.get(usize::from(n)),
            ColumnId::Name => self.name.as_ref(),
            _ => None,
        }
    }

    /// The column that absorbs the leftover width: the plan's choice if it
    /// named one this plan actually draws, else Name.
    #[must_use]
    pub fn flex_column(&self) -> ColumnId {
        match self.flex {
            Some(id) if self.columns.contains(&id) => id,
            _ => ColumnId::Name,
        }
    }

    /// What the header row says for `id`: the plan's word for its own columns
    /// and for a renamed Name, the panel's for the rest.
    #[must_use]
    pub fn header(&self, id: ColumnId) -> &str {
        self.custom(id)
            .map_or_else(|| id.header(), |c| c.header.as_str())
    }
}

/// The configured order, de-duplicated, with `name` guaranteed present.
///
/// the design calls `name` "always present"; a configuration that omits it
/// gets it back at the front rather than a panel with no filenames in it.
pub fn effective_order(configured: &[ColumnId]) -> Vec<ColumnId> {
    let mut order: Vec<ColumnId> = Vec::with_capacity(configured.len().saturating_add(1));
    if !configured.contains(&ColumnId::Name) {
        order.push(ColumnId::Name);
    }
    for id in configured {
        if !order.contains(id) {
            order.push(*id);
        }
    }
    order
}

/// Lay out one panel's columns for an interior `inner_width` cells wide.
///
///
/// `inner_width` is the space inside the panel's borders. Zero, one and two are
/// legal inputs and produce a `name`-only allocation rather than a panic.
pub fn allocate(cfg: &PanelConfig, inner_width: usize, plan: Option<&ColumnPlan>) -> Allocation {
    let order = effective_order(plan.map_or(&cfg.columns.order, |p| &p.columns));
    let align = |id: ColumnId| {
        plan.and_then(|p| p.custom(id))
            .map_or_else(|| super::format::align_of(id), |c| c.align)
    };
    // A grid packs its columns and leaves the leftover empty; only a panel with
    // one long column stretches it.
    if plan.is_some_and(|p| p.pack) {
        return allocate_packed(cfg, inner_width, plan, &order, &align);
    }
    // The column that takes the leftover and is never dropped: Name, unless a
    // plan nominated a data column so its own short id column stays narrow.
    let flex = plan.map_or(ColumnId::Name, ColumnPlan::flex_column);
    // The flex column's floor. Name's is the configured `name_min_width`; a
    // custom flex column's is its own `min_chars`.
    let name_min = if flex == ColumnId::Name {
        usize::from(cfg.effective_name_min_width())
    } else {
        plan.and_then(|p| p.custom(flex))
            .map_or(1, |c| usize::from(c.min_chars))
    };
    let mut hidden: Vec<ColumnId> = Vec::new();

    loop {
        // Step 1, for every column that is still in the running.
        let mut fixed: Vec<Allocated> = Vec::new();
        for id in &order {
            if *id == flex || hidden.contains(id) {
                continue;
            }
            let width = requested_width(cfg, *id, inner_width, plan);
            if width == 0 {
                // Neither a percentage nor a minimum: nothing to draw.
                hidden.push(*id);
                continue;
            }
            fixed.push(Allocated {
                id: *id,
                width,
                align: align(*id),
            });
        }

        let separators = fixed.len().saturating_mul(SEPARATOR_WIDTH);
        let fixed_total: usize = fixed.iter().map(|c| c.width).sum();
        let needed = fixed_total
            .saturating_add(separators)
            .saturating_add(name_min);

        // Steps 2 and 3: `name` takes the leftover, and the layout is accepted
        // only if that leftover still clears `name_min_width`. With no fixed
        // columns left there is nothing further to hide, so `name` takes the
        // whole interior however narrow it is - it is never dropped.
        if fixed.is_empty() || needed <= inner_width {
            let name_width = inner_width
                .saturating_sub(fixed_total)
                .saturating_sub(separators);
            let mut columns = Vec::with_capacity(fixed.len().saturating_add(1));
            for id in &order {
                if *id == flex {
                    columns.push(Allocated {
                        id: *id,
                        width: name_width,
                        align: align(*id),
                    });
                } else if let Some(col) = fixed.iter().find(|c| c.id == *id) {
                    columns.push(*col);
                }
            }
            let ext_visible = columns.iter().any(|c| c.id == ColumnId::Ext);
            let crop = match cfg.effective_name_truncate() {
                NameTruncate::End => Crop::End,
                NameTruncate::Middle => Crop::Middle,
                // decided from what is actually rendered.
                NameTruncate::Auto if ext_visible => Crop::End,
                NameTruncate::Auto => Crop::Middle,
            };
            return Allocation {
                columns,
                inner_width,
                crop,
            };
        }

        // Steps 3 and 4: hide one column and start again.
        //
        // `hide_priority` decides which. A column that is in `order` but not in
        // `hide_priority` would otherwise be unhideable and the loop would not
        // terminate, so the fallback drops the rightmost still-visible column -
        // the same rule, applied to a configuration that forgot to rank it.
        let victim = cfg
            .columns
            .hide_priority
            .iter()
            .copied()
            .find(|c| *c != flex && order.contains(c) && !hidden.contains(c))
            .or_else(|| {
                order
                    .iter()
                    .rev()
                    .copied()
                    .find(|c| *c != flex && !hidden.contains(c))
            });
        match victim {
            Some(id) => hidden.push(id),
            // Unreachable: `fixed` is non-empty here, so at least one non-name
            // column is neither hidden nor absent from `order`, and the
            // `or_else` arm finds it. Returning beats looping for ever.
            None => {
                return Allocation {
                    columns: vec![Allocated {
                        id: ColumnId::Name,
                        width: inner_width,
                        align: Align::Left,
                    }],
                    inner_width,
                    crop: Crop::Middle,
                };
            }
        }
    }
}

/// Lay out a grid: each column at its own width, packed from the left, the
/// leftover left empty. No column is stretched across the panel, which is the
/// mid-table gap a database table would otherwise show. The Name column - the
/// row id - is still never dropped; when even it does not fit it is capped to
/// the interior so the layout never overflows.
fn allocate_packed<F: Fn(ColumnId) -> Align>(
    cfg: &PanelConfig,
    inner_width: usize,
    plan: Option<&ColumnPlan>,
    order: &[ColumnId],
    align: &F,
) -> Allocation {
    let mut hidden: Vec<ColumnId> = Vec::new();
    let columns = loop {
        let mut cols: Vec<Allocated> = Vec::new();
        for id in order {
            if hidden.contains(id) {
                continue;
            }
            let width = requested_width(cfg, *id, inner_width, plan);
            if width == 0 {
                hidden.push(*id);
                continue;
            }
            cols.push(Allocated {
                id: *id,
                width,
                align: align(*id),
            });
        }
        let separators = cols.len().saturating_sub(1).saturating_mul(SEPARATOR_WIDTH);
        let total = cols
            .iter()
            .map(|c| c.width)
            .sum::<usize>()
            .saturating_add(separators);
        if total <= inner_width {
            break cols;
        }
        // Too wide: drop the lowest-priority column, never the id/Name one.
        let victim = cfg
            .columns
            .hide_priority
            .iter()
            .copied()
            .find(|c| *c != ColumnId::Name && order.contains(c) && !hidden.contains(c))
            .or_else(|| {
                order
                    .iter()
                    .rev()
                    .copied()
                    .find(|c| *c != ColumnId::Name && !hidden.contains(c))
            });
        match victim {
            Some(id) => hidden.push(id),
            // Only the id column is left and it still overflows: cap it to the
            // interior rather than draw past the border.
            None => {
                break vec![Allocated {
                    id: ColumnId::Name,
                    width: inner_width,
                    align: align(ColumnId::Name),
                }];
            }
        }
    };
    let ext_visible = columns.iter().any(|c| c.id == ColumnId::Ext);
    let crop = match cfg.effective_name_truncate() {
        NameTruncate::End => Crop::End,
        NameTruncate::Middle => Crop::Middle,
        NameTruncate::Auto if ext_visible => Crop::End,
        NameTruncate::Auto => Crop::Middle,
    };
    Allocation {
        columns,
        inner_width,
        crop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn cfg() -> PanelConfig {
        Config::default().panel
    }

    fn visible(inner: usize) -> Vec<ColumnId> {
        allocate(&cfg(), inner, None)
            .columns()
            .iter()
            .map(|c| c.id)
            .collect()
    }

    #[test]
    fn the_total_never_exceeds_the_inner_width_at_any_width() {
        let cfg = cfg();
        for inner in 0..=200usize {
            let a = allocate(&cfg, inner, None);
            assert!(
                a.total_width() <= inner,
                "inner {inner}: total {} > {inner} ({:?})",
                a.total_width(),
                a.columns()
            );
            assert!(
                a.is_visible(ColumnId::Name),
                "inner {inner}: name is never dropped"
            );
            assert!(
                a.columns()
                    .iter()
                    .all(|c| c.id == ColumnId::Name || c.width > 0),
                "inner {inner}: a visible column with zero width"
            );
        }
    }

    #[test]
    fn zero_one_and_two_are_name_only_and_do_not_panic() {
        for inner in [0usize, 1, 2] {
            let a = allocate(&cfg(), inner, None);
            assert_eq!(a.columns().len(), 1);
            assert_eq!(a.name_width(), inner);
            assert!(a.total_width() <= inner);
        }
    }

    #[test]
    fn a_narrowing_panel_loses_attr_then_ext_then_size_and_keeps_date_longest() {
        // Acceptance criterion 7. Walk from wide to narrow and
        // record the width at which each column disappears for good.
        let mut lost: Vec<ColumnId> = Vec::new();
        let mut previous = visible(200);
        assert_eq!(
            previous,
            vec![
                ColumnId::Name,
                ColumnId::Ext,
                ColumnId::Size,
                ColumnId::Date,
                ColumnId::Attr,
                ColumnId::GitState
            ],
            "everything fits at 200"
        );

        for inner in (0..200usize).rev() {
            let now = visible(inner);
            for id in &previous {
                if !now.contains(id) && !lost.contains(id) {
                    lost.push(*id);
                }
            }
            // Once a column is gone it never comes back as the panel narrows
            // further: allocation is monotone in the inner width.
            for id in &lost {
                assert!(
                    !now.contains(id),
                    "{id} reappeared at inner width {inner}: {now:?}"
                );
            }
            previous = now;
        }

        assert_eq!(
            lost,
            vec![
                ColumnId::Attr,
                ColumnId::Ext,
                ColumnId::Size,
                ColumnId::Date,
                ColumnId::GitState
            ],
            "the default hide_priority order"
        );
    }

    #[test]
    fn name_never_falls_below_its_minimum_while_a_column_could_be_hidden() {
        let cfg = cfg();
        let floor = usize::from(cfg.effective_name_min_width());
        for inner in 0..=200usize {
            let a = allocate(&cfg, inner, None);
            if a.columns().len() > 1 {
                assert!(
                    a.name_width() >= floor,
                    "inner {inner}: name {} < {floor} with {:?} still shown",
                    a.name_width(),
                    a.columns()
                );
            }
        }
    }

    #[test]
    fn a_column_that_cannot_reach_min_chars_is_hidden_not_squeezed() {
        let cfg = cfg();
        for inner in 0..=200usize {
            let a = allocate(&cfg, inner, None);
            for col in a.columns() {
                if col.id == ColumnId::Name {
                    continue;
                }
                let min = default_min_chars(&cfg, col.id);
                assert!(
                    col.width >= min,
                    "inner {inner}: {} rendered at {} < {min}",
                    col.id,
                    col.width
                );
            }
        }
    }

    #[test]
    fn auto_cropping_follows_whether_ext_is_actually_rendered() {
        let cfg = cfg();
        for inner in 0..=200usize {
            let a = allocate(&cfg, inner, None);
            let expected = if a.is_visible(ColumnId::Ext) {
                Crop::End
            } else {
                Crop::Middle
            };
            assert_eq!(a.crop(), expected, "inner {inner}");
        }
        // And there really is a crossover, so the assertion above is not vacuous.
        assert_eq!(allocate(&cfg, 100, None).crop(), Crop::End);
        assert_eq!(allocate(&cfg, 30, None).crop(), Crop::Middle);
    }

    #[test]
    fn an_explicit_truncate_setting_overrides_the_auto_rule() {
        let mut cfg = cfg();
        cfg.name_truncate = Some(NameTruncate::Middle);
        assert_eq!(allocate(&cfg, 120, None).crop(), Crop::Middle);
        cfg.name_truncate = Some(NameTruncate::End);
        assert_eq!(allocate(&cfg, 20, None).crop(), Crop::End);
    }

    #[test]
    fn reordering_the_columns_reorders_the_allocation() {
        let mut cfg = cfg();
        cfg.columns.order = vec![
            ColumnId::Size,
            ColumnId::Name,
            ColumnId::Date,
            ColumnId::Ext,
        ];
        let a = allocate(&cfg, 120, None);
        let ids: Vec<ColumnId> = a.columns().iter().map(|c| c.id).collect();
        assert_eq!(
            ids,
            vec![
                ColumnId::Size,
                ColumnId::Name,
                ColumnId::Date,
                ColumnId::Ext
            ]
        );
    }

    #[test]
    fn a_configuration_without_name_still_gets_a_name_column() {
        let mut cfg = cfg();
        cfg.columns.order = vec![ColumnId::Size, ColumnId::Date];
        let a = allocate(&cfg, 100, None);
        assert_eq!(
            a.columns().first().map(|c| c.id),
            Some(ColumnId::Name),
            "name is prepended when the configuration forgets it"
        );
    }

    #[test]
    fn an_unranked_column_is_still_hideable_so_allocation_terminates() {
        let mut cfg = cfg();
        cfg.columns.order = vec![ColumnId::Name, ColumnId::Owner, ColumnId::Group];
        cfg.columns.hide_priority = Vec::new();
        for inner in 0..=80usize {
            let a = allocate(&cfg, inner, None);
            assert!(a.total_width() <= inner, "inner {inner}");
        }
        // Wide enough for name + owner + group, narrow enough for none of them.
        assert!(allocate(&cfg, 60, None).is_visible(ColumnId::Group));
        assert!(!allocate(&cfg, 20, None).is_visible(ColumnId::Group));
    }

    #[test]
    fn a_partial_min_chars_table_behaves_like_the_full_one() {
        // `min_chars` in config.toml replaces the compiled-in table rather than
        // merging with it, so a user tuning three columns leaves `attr` on the
        // fallback. The fallback has to agree with the compiled-in default, or
        // `attr` is kept at a width that can only render `drwxr…` - the
        // half-drawn column the design rules out.
        let full = cfg();
        let mut partial = cfg();
        partial.columns.min_chars = std::collections::HashMap::from([
            (ColumnId::Ext, 3),
            (ColumnId::Size, 7),
            (ColumnId::Date, 16),
        ]);
        for inner in 0..=200usize {
            let a = allocate(&full, inner, None);
            let b = allocate(&partial, inner, None);
            assert_eq!(
                a.width_of(ColumnId::Attr),
                b.width_of(ColumnId::Attr),
                "inner {inner}"
            );
        }
        // ...and with the four-character DOS style, four is right again.
        let mut dos = partial.clone();
        dos.attr_style = AttrStyle::Dos;
        assert_eq!(default_min_chars(&dos, ColumnId::Attr), 4);
        assert_eq!(default_min_chars(&partial, ColumnId::Attr), 10);
    }

    #[test]
    fn owner_and_group_render_without_extra_configuration() {
        let mut cfg = cfg();
        cfg.columns.order = vec![ColumnId::Name, ColumnId::Owner];
        let a = allocate(&cfg, 100, None);
        assert_eq!(
            a.width_of(ColumnId::Owner),
            Some(default_min_chars(&cfg, ColumnId::Owner))
        );
    }
}

#[cfg(test)]
mod plan_tests {
    use super::*;
    use crate::config::Config;

    fn cfg() -> PanelConfig {
        Config::default().panel
    }

    fn table_plan() -> ColumnPlan {
        ColumnPlan {
            columns: vec![ColumnId::Name, ColumnId::Custom(0), ColumnId::Custom(1)],
            custom: vec![
                CustomColumn {
                    header: "firstname".to_string(),
                    align: Align::Left,
                    min_chars: 12,
                },
                CustomColumn {
                    header: "age".to_string(),
                    align: Align::Right,
                    min_chars: 4,
                },
            ],
            name: None,
            flex: None,
            pack: false,
        }
    }

    #[test]
    fn a_column_the_listing_defined_is_laid_out_from_its_own_words() {
        // The panel has no idea what `firstname` is. It draws it at the width
        // the plan asked for, sitting the way the plan said, under the header
        // the plan gave it - and never learns the word.
        let plan = table_plan();
        let a = allocate(&cfg(), 80, Some(&plan));
        let first = a.columns().iter().find(|c| c.id == ColumnId::Custom(0));
        let age = a.columns().iter().find(|c| c.id == ColumnId::Custom(1));
        assert_eq!(first.map(|c| (c.width, c.align)), Some((12, Align::Left)));
        assert_eq!(age.map(|c| (c.width, c.align)), Some((4, Align::Right)));
        assert_eq!(plan.header(ColumnId::Custom(0)), "firstname");
        assert_eq!(plan.header(ColumnId::Custom(1)), "age");
        assert_eq!(
            plan.header(ColumnId::Name),
            "Name",
            "the panel's own stay its own"
        );
        assert_eq!(
            a.name_width(),
            80 - 12 - 4 - 2 * SEPARATOR_WIDTH,
            "and name takes what is left, as it always has"
        );
    }

    #[test]
    fn a_custom_column_narrows_and_drops_by_the_same_rules_as_the_rest() {
        let plan = table_plan();
        for inner in 0..=120usize {
            let a = allocate(&cfg(), inner, Some(&plan));
            assert!(a.total_width() <= inner, "inner {inner}: {a:?}");
            assert!(a.is_visible(ColumnId::Name), "name is never dropped");
        }
    }

    #[test]
    fn a_plan_that_forgot_a_definition_still_draws_something() {
        // `Custom(3)` with only two definitions: the floor, not a panic.
        let plan = ColumnPlan {
            columns: vec![ColumnId::Name, ColumnId::Custom(3)],
            custom: Vec::new(),
            name: None,
            flex: None,
            pack: false,
        };
        let a = allocate(&cfg(), 60, Some(&plan));
        assert!(a.is_visible(ColumnId::Custom(3)));
    }

    #[test]
    fn a_renamed_name_column_stays_narrow_and_a_data_column_takes_the_room() {
        // A database table names its first column `id` and hands its width to
        // `firstname`: the id column is exactly as wide as the plan asked, its
        // header is `id` and not "Name", and `firstname` absorbs the leftover
        // rather than the id doing it.
        let plan = ColumnPlan {
            columns: vec![ColumnId::Name, ColumnId::Custom(0)],
            custom: vec![CustomColumn {
                header: "firstname".to_string(),
                align: Align::Left,
                min_chars: 12,
            }],
            name: Some(CustomColumn {
                header: "id".to_string(),
                align: Align::Left,
                min_chars: 6,
            }),
            flex: Some(ColumnId::Custom(0)),
            pack: false,
        };
        let a = allocate(&cfg(), 80, Some(&plan));
        assert_eq!(
            a.name_width(),
            6,
            "the id column keeps the width it asked for"
        );
        assert_eq!(plan.header(ColumnId::Name), "id", "and its own name");
        let first = a
            .columns()
            .iter()
            .find(|c| c.id == ColumnId::Custom(0))
            .map(|c| c.width);
        assert_eq!(
            first,
            Some(80 - 6 - SEPARATOR_WIDTH),
            "firstname takes the leftover, the id does not"
        );
    }

    #[test]
    fn a_nominated_flex_column_is_never_dropped_and_the_layout_fits() {
        let plan = ColumnPlan {
            columns: vec![ColumnId::Name, ColumnId::Custom(0)],
            custom: vec![CustomColumn {
                header: "firstname".to_string(),
                align: Align::Left,
                min_chars: 12,
            }],
            name: Some(CustomColumn {
                header: "id".to_string(),
                align: Align::Left,
                min_chars: 6,
            }),
            flex: Some(ColumnId::Custom(0)),
            pack: false,
        };
        for inner in 0..=120usize {
            let a = allocate(&cfg(), inner, Some(&plan));
            assert!(a.total_width() <= inner, "inner {inner}: {a:?}");
            assert!(
                a.is_visible(ColumnId::Custom(0)),
                "inner {inner}: the flex column is never dropped",
            );
        }
    }

    fn packed_grid() -> ColumnPlan {
        ColumnPlan {
            columns: vec![ColumnId::Name, ColumnId::Custom(0), ColumnId::Custom(1)],
            custom: vec![
                CustomColumn {
                    header: "birth_date".to_string(),
                    align: Align::Left,
                    min_chars: 11,
                },
                CustomColumn {
                    header: "gender".to_string(),
                    align: Align::Left,
                    min_chars: 7,
                },
            ],
            name: Some(CustomColumn {
                header: "id".to_string(),
                align: Align::Left,
                min_chars: 6,
            }),
            flex: None,
            pack: true,
        }
    }

    #[test]
    fn a_packed_grid_takes_each_columns_own_width_and_leaves_the_rest_empty() {
        // The gap a database table used to show: no column stretches, so on a
        // wide panel the columns keep their own widths and the leftover is
        // simply not allocated.
        let plan = packed_grid();
        let a = allocate(&cfg(), 200, Some(&plan));
        assert_eq!(a.width_of(ColumnId::Name), Some(6), "the id stays narrow");
        assert_eq!(a.width_of(ColumnId::Custom(0)), Some(11), "not stretched");
        assert_eq!(a.width_of(ColumnId::Custom(1)), Some(7), "nor this one");
        assert!(
            a.total_width() < 200,
            "the leftover is empty, not given to a column: {}",
            a.total_width()
        );
    }

    #[test]
    fn a_packed_grid_never_overflows_and_keeps_the_id_column() {
        let plan = packed_grid();
        for inner in 0..=120usize {
            let a = allocate(&cfg(), inner, Some(&plan));
            assert!(a.total_width() <= inner, "inner {inner}: {a:?}");
            assert!(
                a.is_visible(ColumnId::Name),
                "inner {inner}: the id column is never dropped",
            );
        }
    }
}
