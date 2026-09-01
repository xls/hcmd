//! The renderer's view of column allocation and cell formatting.
//!
//!
//! # This module is a forward
//!
//! Every rule here - percentage allocation, `min_chars`, `hide_priority`, the
//! `name_min_width` re-run, the `auto` crop rule and the size/date/attr
//! formatters - lives in [`crate::panel::columns`] and [`crate::panel::format`].
//! It belongs there rather than here because the design makes the *model* need
//! the same answers the renderer draws: the panel status line shows the full
//! name of the entry under the cursor precisely when its cell cropped it, so
//! "what was allocated" and "did it fit" are model questions.
//!
//! the design split the two across phase 2b and phase 2d, which were
//! written at the same time, so both ended up implementing the design. The
//! two implementations disagreed - on where a middle crop splits, on the DOS
//! attribute of a symlink, on the spacing of a human-readable size, and on what
//! to allocate below `name_min_width`, where this one returned columns totalling
//! more than the panel was wide. These are forwards so there is one answer.
//!
//! What stays here is what is genuinely about painting: [`Glyphs`] (the ASCII
//! fallback table) and turning an [`Allocated`] plus a [`Glyphs`] into a padded
//! string.

use crate::config::{NameTruncate, PanelConfig};
use crate::panel::{ColumnId, format};
use crate::vfs::Entry;

use super::text::{self, Crop, Glyphs};

pub use crate::panel::text::Align;
pub use crate::panel::{Allocated, allocate as allocate_full};

/// Allocate the configured columns across `inner_width` cells.
///
/// The list is in configured `order`, so `Ctrl+<n>` addressing a hidden column
/// is unaffected.
///
/// `plan` is what a listing asked for instead of the configured set, and is
/// `None` for an ordinary directory, which wants what the user configured. It
/// replaces the order and nothing else: the widths, the hide-by-priority order
/// and the name minimum are all still the configuration's business, so a
/// backend names its columns without taking on the layout.
pub fn allocate(
    cfg: &PanelConfig,
    inner_width: usize,
    plan: Option<&[ColumnId]>,
) -> Vec<Allocated> {
    let Some(plan) = plan else {
        return allocate_full(cfg, inner_width).columns().to_vec();
    };
    let mut planned = cfg.clone();
    planned.columns.order = plan.to_vec();
    allocate_full(&planned, inner_width).columns().to_vec()
}

/// How a column's text sits in its cell (`size` is the
/// right-aligned one).
pub const fn align_of(id: ColumnId) -> Align {
    format::align_of(id)
}

/// The configured order, de-duplicated, with `name` guaranteed present
/// (`name` is never dropped).
pub fn effective_order(cfg: &PanelConfig) -> Vec<ColumnId> {
    crate::panel::columns::effective_order(&cfg.columns.order)
}

/// How the `name` column crops, resolved against what is *actually rendered*
/// (`auto` end-crops when `ext` is a rendered column, because the
/// extension is then already displayed separately).
pub fn name_crop(cfg: &PanelConfig, allocated: &[Allocated]) -> Crop {
    match cfg.effective_name_truncate() {
        NameTruncate::Middle => Crop::Middle,
        NameTruncate::End => Crop::End,
        NameTruncate::Auto => {
            if allocated.iter().any(|a| a.id == ColumnId::Ext) {
                Crop::End
            } else {
                Crop::Middle
            }
        }
    }
}

/// The header text for a column, with the sort arrow prefixed to the sorted
/// one as Total Commander does it.
pub fn header_text(id: ColumnId, sorted: bool, reverse: bool, g: Glyphs) -> String {
    if sorted {
        format!("{}{}", g.arrow(reverse), id.header())
    } else {
        id.header().to_string()
    }
}

/// The untruncated text of one cell.
///
/// `ext_rendered` says whether the `ext` column survived allocation: when it
/// did, the extension is displayed separately and the `name` cell carries only
/// the stem. When it did not, the name carries the whole thing.
pub fn cell_text(
    entry: &Entry,
    id: ColumnId,
    cfg: &PanelConfig,
    ext_rendered: bool,
    local_ids: bool,
) -> String {
    format::cell_text(entry, id, cfg, ext_rendered, local_ids)
}

/// [`cell_text`] with a directory's walked size.
///
/// See [`format::cell_text_with`]: `sized` fills in the `size` column for a
/// directory that `Space` or `Ctrl+L` has walked, so the row and the status
/// line agree about it.
pub fn cell_text_with(
    entry: &Entry,
    id: ColumnId,
    cfg: &PanelConfig,
    ext_rendered: bool,
    sized: Option<u64>,
    local_ids: bool,
) -> String {
    format::cell_text_with(entry, id, cfg, ext_rendered, sized, local_ids)
}

/// A byte count with thousands separators - `362,333`.
pub fn group_digits(n: u64) -> String {
    format::thousands(n)
}

/// Format one cell to exactly `column.width` cells.
pub fn fit_cell(body: &str, column: Allocated, crop: Crop, g: Glyphs) -> String {
    match align_of(column.id) {
        Align::Left => text::fit_left(body, column.width, crop, g.ellipsis()),
        Align::Right => text::fit_right(body, column.width, crop, g.ellipsis()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn cfg() -> PanelConfig {
        Config::default().panel
    }

    #[test]
    fn a_wide_panel_shows_every_configured_column() {
        let got = allocate(&cfg(), 120, None);
        let ids: Vec<ColumnId> = got.iter().map(|a| a.id).collect();
        assert_eq!(
            ids,
            [
                ColumnId::Name,
                ColumnId::Ext,
                ColumnId::Size,
                ColumnId::Date,
                ColumnId::Attr,
                ColumnId::GitState
            ]
        );
    }

    #[test]
    fn a_listing_that_names_its_columns_gets_those_and_no_others() {
        // The seam: a backend says what its rows want and the configured set
        // steps aside. Nothing else about the layout is the backend's to know.
        let plan = [ColumnId::Name, ColumnId::Date];
        let got = allocate(&cfg(), 120, Some(&plan));
        let ids: Vec<ColumnId> = got.iter().map(|a| a.id).collect();
        assert_eq!(ids, [ColumnId::Name, ColumnId::Date]);
        assert!(
            !ids.contains(&ColumnId::Ext) && !ids.contains(&ColumnId::Attr),
            "what it did not ask for is not drawn: {ids:?}"
        );
    }

    #[test]
    fn a_plan_still_narrows_by_the_configured_rules() {
        // It names columns, not widths: hiding as the panel narrows and the
        // name minimum stay the configuration's business, so a backend cannot
        // produce a layout that does not fit.
        let plan = [
            ColumnId::Name,
            ColumnId::Size,
            ColumnId::Date,
            ColumnId::GitState,
        ];
        let narrow = allocate(&cfg(), 24, Some(&plan));
        let total: usize = narrow.iter().map(|a| a.width).sum();
        assert!(total <= 24, "it fits: {narrow:?}");
        assert!(
            narrow.iter().any(|a| a.id == ColumnId::Name),
            "and the name is never dropped: {narrow:?}"
        );
    }

    #[test]
    fn the_allocation_never_overflows_the_panel() {
        // The bug this forward removed: the old local allocator returned every
        // column, totalling 31 cells, for a panel one cell wide.
        let cfg = cfg();
        for w in 0..=200usize {
            let got = allocate(&cfg, w, None);
            let sep = got.len().saturating_sub(1);
            let total: usize = got.iter().map(|a| a.width).sum::<usize>() + sep;
            assert!(total <= w, "width {w}: allocated {total} for {got:?}");
            assert!(
                got.iter().any(|a| a.id == ColumnId::Name),
                "width {w}: `name` is never dropped"
            );
        }
    }

    #[test]
    fn the_auto_crop_follows_whether_ext_is_rendered() {
        let cfg = cfg();
        let wide = allocate(&cfg, 120, None);
        assert_eq!(name_crop(&cfg, &wide), Crop::End, "ext is rendered");
        let narrow = allocate(&cfg, 36, None);
        assert!(!narrow.iter().any(|a| a.id == ColumnId::Ext));
        assert_eq!(name_crop(&cfg, &narrow), Crop::Middle, "ext is hidden");
    }

    #[test]
    fn size_is_the_right_aligned_column() {
        assert_eq!(align_of(ColumnId::Size), Align::Right);
        assert_eq!(align_of(ColumnId::Name), Align::Left);
        assert_eq!(align_of(ColumnId::Date), Align::Left);
    }
}
