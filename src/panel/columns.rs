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
    }
}

/// Step 1: `round(pct × inner_width)`, clamped up to `min_chars`.
fn requested_width(cfg: &PanelConfig, id: ColumnId, inner_width: usize) -> usize {
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

/// The configured order, with `name` guaranteed present.
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
pub fn allocate(cfg: &PanelConfig, inner_width: usize) -> Allocation {
    let order = effective_order(&cfg.columns.order);
    let name_min = usize::from(cfg.effective_name_min_width());
    let mut hidden: Vec<ColumnId> = Vec::new();

    loop {
        // Step 1, for every column that is still in the running.
        let mut fixed: Vec<Allocated> = Vec::new();
        for id in &order {
            if *id == ColumnId::Name || hidden.contains(id) {
                continue;
            }
            let width = requested_width(cfg, *id, inner_width);
            if width == 0 {
                // Neither a percentage nor a minimum: nothing to draw.
                hidden.push(*id);
                continue;
            }
            fixed.push(Allocated { id: *id, width });
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
                if *id == ColumnId::Name {
                    columns.push(Allocated {
                        id: ColumnId::Name,
                        width: name_width,
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
            // Unreachable: `fixed` is non-empty here, so at least one non-name
            // column is neither hidden nor absent from `order`, and the
            // `or_else` arm finds it. Returning beats looping for ever.
            None => {
                return Allocation {
                    columns: vec![Allocated {
                        id: ColumnId::Name,
                        width: inner_width,
                    }],
                    inner_width,
                    crop: Crop::Middle,
                };
            }
        }
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
        allocate(&cfg(), inner)
            .columns()
            .iter()
            .map(|c| c.id)
            .collect()
    }

    #[test]
    fn the_total_never_exceeds_the_inner_width_at_any_width() {
        let cfg = cfg();
        for inner in 0..=200usize {
            let a = allocate(&cfg, inner);
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
            let a = allocate(&cfg(), inner);
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
                ColumnId::Attr
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
                ColumnId::Date
            ],
            "the default hide_priority order"
        );
    }

    #[test]
    fn name_never_falls_below_its_minimum_while_a_column_could_be_hidden() {
        let cfg = cfg();
        let floor = usize::from(cfg.effective_name_min_width());
        for inner in 0..=200usize {
            let a = allocate(&cfg, inner);
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
            let a = allocate(&cfg, inner);
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
            let a = allocate(&cfg, inner);
            let expected = if a.is_visible(ColumnId::Ext) {
                Crop::End
            } else {
                Crop::Middle
            };
            assert_eq!(a.crop(), expected, "inner {inner}");
        }
        // And there really is a crossover, so the assertion above is not vacuous.
        assert_eq!(allocate(&cfg, 100).crop(), Crop::End);
        assert_eq!(allocate(&cfg, 30).crop(), Crop::Middle);
    }

    #[test]
    fn an_explicit_truncate_setting_overrides_the_auto_rule() {
        let mut cfg = cfg();
        cfg.name_truncate = Some(NameTruncate::Middle);
        assert_eq!(allocate(&cfg, 120).crop(), Crop::Middle);
        cfg.name_truncate = Some(NameTruncate::End);
        assert_eq!(allocate(&cfg, 20).crop(), Crop::End);
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
        let a = allocate(&cfg, 120);
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
        let a = allocate(&cfg, 100);
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
            let a = allocate(&cfg, inner);
            assert!(a.total_width() <= inner, "inner {inner}");
        }
        // Wide enough for name + owner + group, narrow enough for none of them.
        assert!(allocate(&cfg, 60).is_visible(ColumnId::Group));
        assert!(!allocate(&cfg, 20).is_visible(ColumnId::Group));
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
            let a = allocate(&full, inner);
            let b = allocate(&partial, inner);
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
        let a = allocate(&cfg, 100);
        assert_eq!(
            a.width_of(ColumnId::Owner),
            Some(default_min_chars(&cfg, ColumnId::Owner))
        );
    }
}
